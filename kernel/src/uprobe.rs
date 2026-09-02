//! User-space instruction probe registry and trampoline ownership.
//!
//! The registry is deliberately keyed by the stable file mapping identity and
//! file offset, never by a pathname or a transient user pointer.  Perf, trace
//! and BPF all feed this one registry.  MM installation is kept separate from
//! registration so a probe can exist before a matching executable VMA appears
//! (exec, dlopen, and a later mmap all use the same object key).

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    mem::MaybeUninit,
    sync::atomic::{AtomicU64, Ordering},
};

use axcpu::TrapFrame;
use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::{MappingFlags, PageSize};
use axsync::Mutex;
use iced_x86::{
    BlockEncoder, BlockEncoderOptions, Code, ConditionCode, Decoder, DecoderOptions, FlowControl,
    InstructionBlock, Mnemonic, OpKind, Register,
};
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
use thekernel_linux_arch_x86_64::SEGV_CPERR;
use thekernel_linux_signal::{SignalInfo, Signo};

use crate::{
    file::PerfEvent,
    mm::{
        AddrSpace, Backend, UserMemoryCapability, UserNofaultError, map_usercopy_error,
        try_user_nofault_transaction,
    },
    task::{AsThread, force_signal_current_thread},
};

/// Identity of a file object as represented by a Linux file-backed VMA.
/// Mount and device are part of the key: inode numbers alone are not global.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct UprobeFileKey {
    pub(crate) mount_id: u64,
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ProbeKey {
    file: UprobeFileKey,
    offset: u64,
}

#[derive(Clone, Copy, Debug)]
struct InstalledProbe {
    address: u64,
    key: ProbeKey,
    /// The byte that must be restored when the last consumer leaves.  The MM
    /// overlay writer fills this during installation; no trap path reads user
    /// memory to rediscover it.
    original_byte: u8,
    retprobe: bool,
    plan: InstructionPlan,
}

/// Address-indexed probes plus pre-reserved remap custody. Rust's BTreeMap has
/// no fallible node reservation API, so a destructive mremap cannot safely add
/// a fresh key after commit. The linear side vector is reserved before VMA
/// mutation and participates in every lookup/iteration exactly like the tree.
#[derive(Clone, Default)]
struct InstalledProbes {
    indexed: BTreeMap<u64, InstalledProbe>,
    remapped: Vec<InstalledProbe>,
}

impl InstalledProbes {
    fn try_reserve_remapped(&mut self, additional: usize) -> AxResult<()> {
        self.remapped
            .try_reserve(additional)
            .map_err(|_| AxError::NoMemory)
    }

    fn values(&self) -> impl Iterator<Item = &InstalledProbe> {
        self.indexed.values().chain(self.remapped.iter())
    }

    fn values_mut(&mut self) -> impl Iterator<Item = &mut InstalledProbe> {
        self.indexed.values_mut().chain(self.remapped.iter_mut())
    }

    fn iter(&self) -> impl Iterator<Item = (&u64, &InstalledProbe)> {
        self.indexed
            .iter()
            .chain(self.remapped.iter().map(|probe| (&probe.address, probe)))
    }

    fn get(&self, address: &u64) -> Option<&InstalledProbe> {
        self.remapped
            .iter()
            .find(|probe| probe.address == *address)
            .or_else(|| self.indexed.get(address))
    }

    fn get_mut(&mut self, address: &u64) -> Option<&mut InstalledProbe> {
        if let Some(index) = self
            .remapped
            .iter()
            .position(|probe| probe.address == *address)
        {
            return self.remapped.get_mut(index);
        }
        self.indexed.get_mut(address)
    }

    fn contains_key(&self, address: &u64) -> bool {
        self.get(address).is_some()
    }

    fn insert(&mut self, address: u64, mut probe: InstalledProbe) -> Option<InstalledProbe> {
        probe.address = address;
        if let Some(index) = self
            .remapped
            .iter()
            .position(|installed| installed.address == address)
        {
            return Some(core::mem::replace(&mut self.remapped[index], probe));
        }
        self.indexed.insert(address, probe)
    }

    /// Consumes capacity reserved by `try_reserve_remapped`; never allocates.
    fn insert_remapped(&mut self, address: u64, mut probe: InstalledProbe) {
        probe.address = address;
        if let Some(index) = self
            .remapped
            .iter()
            .position(|installed| installed.address == address)
        {
            self.remapped[index] = probe;
            return;
        }
        self.indexed.remove(&address);
        debug_assert!(self.remapped.len() < self.remapped.capacity());
        self.remapped.push(probe);
    }

    fn remove(&mut self, address: &u64) -> Option<InstalledProbe> {
        let remapped = self
            .remapped
            .iter()
            .position(|probe| probe.address == *address)
            .map(|index| self.remapped.swap_remove(index));
        remapped.or_else(|| self.indexed.remove(address))
    }

    fn retain(&mut self, mut retain: impl FnMut(&u64, &mut InstalledProbe) -> bool) {
        self.indexed.retain(|address, probe| retain(address, probe));
        self.remapped.retain_mut(|probe| {
            let address = probe.address;
            retain(&address, probe)
        });
    }
}

/// Immutable installation-time instruction contract.  Trap handling never
/// refetches user instruction bytes: a concurrent unmap/COW cannot change the
/// decoded length, RIP-relative fixups, or flow-control classification after
/// the INT3 has become visible.
#[derive(Clone, Copy, Debug)]
enum InstructionPlan {
    Relocated {
        bytes: [u8; 15],
        len: u8,
    },
    DirectControl {
        flow: FlowControl,
        target: u64,
        condition: ConditionCode,
        len: u8,
        stack_increment: u32,
    },
    LoopControl {
        mnemonic: Mnemonic,
        target: u64,
        len: u8,
    },
    /// These forms require architectural stack/register emulation (including
    /// CET shadow-stack state) and remain explicitly owned until that engine
    /// is installed. They are SIGILL, never a fall-through user SIGTRAP.
    ControlledUnsupported {
        bytes: [u8; 15],
        len: u8,
    },
    /// Internal-only repair ownership after a changed COW leaf was observed
    /// between predecode and patch. It is never produced for a legitimate
    /// unsupported instruction and is retried by reconciliation.
    RepairPending,
}

#[derive(Clone, Default)]
struct MmProbeState {
    installed: InstalledProbes,
    /// Per-mm USDT semaphore contributions already written into userspace.
    /// The key keeps the instruction probe and semaphore file offsets
    /// separate because several probes may legitimately share one counter.
    reference_counters: BTreeMap<(ProbeKey, u64), AppliedReferenceCounter>,
    /// Counters whose private mapping still exists but is temporarily not
    /// writable.  The deferred worker must not poll these: mprotect/munmap
    /// reconciliation is the event which makes progress possible.
    blocked_reference_counters: BTreeMap<(ProbeKey, u64), CounterBlockState>,
    /// RX-only XOL mapping base.  Zero means this mm has no special mapping.
    xol_base: u64,
    /// The internal syscall trampoline page within `xol_base`.
    trampoline_base: u64,
    /// Changes every time this mm receives a new XOL mapping.  A pending
    /// single step is tied to this identity, not merely to an address that a
    /// later mapping may recycle.
    xol_generation: u64,
    /// Non-forgeable VMA-side identity paired with the generation.
    xol_token: u64,
    /// The registry never owns an MM, but retaining a weak identity lets the
    /// last close restore the private COW overlay in every live participant.
    aspace: Weak<Mutex<AddrSpace>>,
    /// Slots currently executing one relocated instruction.  A slot is never
    /// reused until its owner has consumed the matching #DB/fault transition.
    xol_slots: BTreeMap<u64, u64>,
    /// `uprobe_mmap()` is best effort: a successful VMA publication must not
    /// be undone because its first instrumentation attempt faulted or ran out
    /// of memory. The deferred worker owns a later full reconciliation.
    pending_mmap_reconcile: bool,
}

#[derive(Default)]
struct Registry {
    consumers: BTreeMap<ProbeKey, ConsumerRefs>,
    mms: BTreeMap<u64, MmProbeState>,
}

#[derive(Clone, Copy, Debug)]
struct ReferenceCounterRefs {
    offset: u64,
    count: u32,
}

#[derive(Clone, Copy, Debug)]
struct AppliedReferenceCounter {
    address: u64,
    count: u32,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CounterBlockState {
    /// Capacity owned by a prepared fixed replacement. It is not observable
    /// as a real mprotect block and is removed after the final fallible step.
    Reserved,
    Blocked,
}

fn counter_is_blocked(state: &MmProbeState, identity: &(ProbeKey, u64)) -> bool {
    matches!(
        state.blocked_reference_counters.get(identity),
        Some(CounterBlockState::Blocked)
    )
}

#[derive(Default)]
struct ConsumerRefs {
    total: u32,
    returns: u32,
    /// Linux fixes the semaphore displacement for the lifetime of one
    /// inode+probe-offset consumer, including the distinguished zero value.
    /// Consequently a zero/nonzero transition is not a compatible second
    /// registration.
    declared_reference_counter_offset: Option<u64>,
    reference_counters: Vec<ReferenceCounterRefs>,
    /// The final external reference has gone away, but the task-context
    /// restore owner still holds the installed overlay custody records.
    retiring: bool,
    /// Mutable pt_regs consumers are bounded because the trampoline path is
    /// allocation-free and may run from exception context.
    entry_handlers: [Option<UprobeFrameHandler>; 8],
    return_handlers: [Option<UprobeFrameHandler>; 8],
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    consumers: BTreeMap::new(),
    mms: BTreeMap::new(),
});
/// Serializes global consumer publication against every MM topology edge
/// which can discover that consumer.  The gate is always acquired before an
/// address-space lock; registry access remains in the established mm->registry
/// order inside the transaction.
static REGISTRATION_GATE: Mutex<()> = Mutex::new(());
static NEXT_OVERLAY_GENERATION: AtomicU64 = AtomicU64::new(1);
static NEXT_XOL_TOKEN: AtomicU64 = AtomicU64::new(1);

pub(crate) fn registration_topology_gate() -> axsync::MutexGuard<'static, ()> {
    REGISTRATION_GATE.lock()
}

fn desired_reference_counter(registry: &Registry, key: ProbeKey, offset: u64) -> u32 {
    registry
        .consumers
        .get(&key)
        .and_then(|refs| {
            refs.reference_counters
                .iter()
                .find(|counter| counter.offset == offset)
        })
        .map_or(0, |counter| counter.count)
}

fn add_active_counter_identities(
    registry: &Registry,
    key: ProbeKey,
    counter_identities: &mut Vec<(ProbeKey, u64)>,
) {
    if let Some(refs) = registry.consumers.get(&key) {
        for counter in refs
            .reference_counters
            .iter()
            .filter(|counter| counter.count != 0)
        {
            let identity = (key, counter.offset);
            if !counter_identities.contains(&identity) {
                // Capacity is admitted from the complete consumer registry
                // before this helper is called.
                debug_assert!(counter_identities.len() < counter_identities.capacity());
                counter_identities.push(identity);
            }
        }
    }
}

fn desired_reference_counter_for_mm(
    registry: &Registry,
    mm_id: u64,
    key: ProbeKey,
    offset: u64,
) -> u32 {
    let installed = registry
        .mms
        .get(&mm_id)
        .is_some_and(|state| state.installed.values().any(|probe| probe.key == key));
    installed
        .then(|| desired_reference_counter(registry, key, offset))
        .unwrap_or(0)
}

enum ReferenceCounterMapping {
    Writable(u64),
    TemporarilyReadOnly,
    Absent,
}

fn mapped_reference_counter(
    aspace: &AddrSpace,
    file: UprobeFileKey,
    offset: u64,
) -> ReferenceCounterMapping {
    let mut read_only = false;
    let address = aspace.areas().find_map(|area| {
        let mapping = area.backend().file_mapping()?;
        let identity = mapping.identity();
        if identity.mount_id() != file.mount_id
            || identity.device() != file.device
            || identity.inode() != file.inode
        {
            return None;
        }
        if mapping.sharing() != crate::mm::FileMappingSharing::Private {
            return None;
        }
        let first = mapping.file_offset_at(area.start())?;
        let length = u64::try_from(area.end().as_usize() - area.start().as_usize()).ok()?;
        if offset < first || offset.checked_add(2)? > first.checked_add(length)? {
            return None;
        }
        if !area.flags().contains(MappingFlags::WRITE) {
            read_only = true;
            return None;
        }
        Some(area.start().as_usize() as u64 + (offset - first))
    });
    match address {
        Some(address) => ReferenceCounterMapping::Writable(address),
        None if read_only => ReferenceCounterMapping::TemporarilyReadOnly,
        None => ReferenceCounterMapping::Absent,
    }
}

/// Bring one mm's user-visible USDT semaphore contribution to the registry's
/// desired count.  The mm lock serializes COW/population and the registry is
/// sampled only in the established mm -> registry order.  If unregister races
/// the write, the loop observes the new target and applies the compensating
/// decrement before returning.
fn sync_reference_counter_locked(
    aspace: &mut AddrSpace,
    key: ProbeKey,
    offset: u64,
    forced_target: Option<u32>,
) -> AxResult<()> {
    if offset == 0 {
        return Ok(());
    }
    let mm_id = aspace.address_space_id().get();
    let address = match mapped_reference_counter(aspace, key.file, offset) {
        ReferenceCounterMapping::Writable(address) => {
            if let Some(state) = REGISTRY.lock().mms.get_mut(&mm_id) {
                state.blocked_reference_counters.remove(&(key, offset));
            }
            address
        }
        // mprotect can temporarily remove write permission while preserving
        // both the private COW page and its already-applied contribution.
        // Forgetting it here would double-increment after PROT_WRITE returns.
        ReferenceCounterMapping::TemporarilyReadOnly => {
            REGISTRY
                .lock()
                .mms
                .entry(mm_id)
                .or_default()
                .blocked_reference_counters
                .insert((key, offset), CounterBlockState::Blocked);
            return Ok(());
        }
        ReferenceCounterMapping::Absent => {
            let removed = if let Some(state) = REGISTRY.lock().mms.get_mut(&mm_id) {
                let removed = state.reference_counters.remove(&(key, offset)).is_some();
                state.blocked_reference_counters.remove(&(key, offset));
                removed
            } else {
                false
            };
            if removed {
                crate::deferred_work::wake_uprobe_restore_worker();
            }
            return Ok(());
        }
    };
    // The byte update below is user-visible.  Publish an inert custody node
    // before population/COW and before that update, so the post-write path is
    // strictly an in-place replacement rather than a fallible BTree insertion.
    // Callers which stage several counters preinsert these nodes earlier; the
    // local admission also makes direct generic-sync users failure-atomic with
    // respect to a missing counter record.
    {
        let mut registry = REGISTRY.lock();
        registry
            .mms
            .entry(mm_id)
            .or_default()
            .reference_counters
            .entry((key, offset))
            .or_insert(AppliedReferenceCounter {
                address: 0,
                count: 0,
            });
    }
    for _ in 0..8 {
        let (desired, current) = {
            let registry = REGISTRY.lock();
            let desired = forced_target
                .unwrap_or_else(|| desired_reference_counter_for_mm(&registry, mm_id, key, offset));
            let current = registry
                .mms
                .get(&mm_id)
                .and_then(|state| state.reference_counters.get(&(key, offset)))
                .filter(|applied| applied.address == address)
                .map_or(0, |applied| applied.count);
            (desired, current)
        };
        if desired == current {
            // Admission may have created an inert zero record solely to
            // guarantee that a later byte update cannot allocate.  If this
            // synchronization is already stably zero, retire that placeholder
            // here; otherwise retired consumers would retain an unobservable
            // reference-counter key forever without scheduling cleanup.
            if desired == 0 {
                let mut registry = REGISTRY.lock();
                let remove_inert = registry
                    .mms
                    .get(&mm_id)
                    .and_then(|state| state.reference_counters.get(&(key, offset)))
                    .is_some_and(|applied| applied.count == 0);
                if remove_inert {
                    registry
                        .mms
                        .get_mut(&mm_id)
                        .expect("counter custody disappeared before inert retirement")
                        .reference_counters
                        .remove(&(key, offset));
                }
            }
            return Ok(());
        }
        let address = usize::try_from(address).map_err(|_| AxError::BadAddress)?;
        let page = VirtAddr::from(address).align_down_4k();
        aspace.populate_area(page, PAGE_SIZE_4K, MappingFlags::WRITE)?;
        let mut bytes = [0u8; 2];
        aspace.read(VirtAddr::from(address), &mut bytes)?;
        let value = u16::from_ne_bytes(bytes);
        let delta = desired.abs_diff(current);
        let delta = u16::try_from(delta).map_err(|_| AxError::InvalidInput)?;
        let value = if desired > current {
            value.checked_add(delta)
        } else {
            value.checked_sub(delta)
        }
        .ok_or(AxError::InvalidInput)?;
        aspace.write(VirtAddr::from(address), &value.to_ne_bytes())?;
        let mut registry = REGISTRY.lock();
        {
            let state = registry
                .mms
                .get_mut(&mm_id)
                .expect("counter custody disappeared after admission");
            let applied = state
                .reference_counters
                .get_mut(&(key, offset))
                .expect("counter custody disappeared before byte publication");
            // This is allocation-free: the custody entry was admitted before
            // the write above.  Keep a zero-count entry across an unstable
            // retry so a newly desired contribution cannot need allocation
            // after a write.
            *applied = AppliedReferenceCounter {
                address: address as u64,
                count: desired,
            };
        }
        let stable = forced_target.is_some()
            || desired_reference_counter_for_mm(&registry, mm_id, key, offset) == desired;
        if desired == 0 && stable {
            registry
                .mms
                .get_mut(&mm_id)
                .expect("counter custody disappeared before retirement")
                .reference_counters
                .remove(&(key, offset));
        }
        drop(registry);
        if desired == 0 {
            crate::deferred_work::wake_uprobe_restore_worker();
        }
        if stable {
            return Ok(());
        }
    }
    crate::deferred_work::wake_uprobe_restore_worker();
    Ok(())
}

fn sync_reference_counter(
    aspace: &Arc<Mutex<AddrSpace>>,
    key: ProbeKey,
    offset: u64,
) -> AxResult<()> {
    let mut guard = aspace.lock();
    let mm_id = guard.address_space_id().get();
    {
        let mut registry = REGISTRY.lock();
        registry.mms.entry(mm_id).or_default().aspace = Arc::downgrade(aspace);
    }
    sync_reference_counter_locked(&mut guard, key, offset, None)
}

/// Maximum nesting matches Linux's bounded return-instance discipline.
pub(crate) const MAX_RETURN_INSTANCES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReturnInstance {
    pub(crate) original_return: u64,
    pub(crate) function: u64,
    key: ProbeKey,
    pub(crate) stack: u64,
    pub(crate) shadow_stack: Option<u64>,
    trampoline: u64,
    /// Only the first record for a return slot changed the normal/SHSTK word.
    /// Tail-call records share that word and must never restore it on rollback.
    owns_trampoline: bool,
    chained: bool,
}

/// Fixed-capacity scratch for a return trampoline chain.  Trap handling must
/// never allocate merely because a tail-call chain is being unwound.
#[derive(Clone, Copy, Eq, PartialEq)]
struct ReturnChain {
    slots: [Option<ReturnInstance>; MAX_RETURN_INSTANCES],
    len: usize,
}

impl ReturnChain {
    const fn new() -> Self {
        Self {
            slots: [None; MAX_RETURN_INSTANCES],
            len: 0,
        }
    }

    fn push(&mut self, instance: ReturnInstance) -> bool {
        if self.len == self.slots.len() {
            return false;
        }
        self.slots[self.len] = Some(instance);
        self.len += 1;
        true
    }

    fn iter(&self) -> impl Iterator<Item = &ReturnInstance> {
        self.slots[..self.len].iter().flatten()
    }

    fn last(&self) -> Option<ReturnInstance> {
        self.len.checked_sub(1).and_then(|index| self.slots[index])
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[derive(Clone)]
struct PendingXol {
    mm_id: u64,
    xol_generation: u64,
    /// Non-forgeable VMA-side identity.  Address and RX flags are only a
    /// location; this token proves the mapping was published by XOL itself.
    xol_token: u64,
    slot: u64,
    expected_end: u64,
    original_tf: bool,
    probe: InstalledProbe,
    aspace: Arc<Mutex<AddrSpace>>,
    resume: u64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct PendingUprobeSyscall {
    mm_id: u64,
    xol_generation: u64,
    xol_token: u64,
    entry: u64,
    return_address: u64,
    syscall_stack: u64,
    key: ProbeKey,
    /// A fork initially clones the architectural return word.  The child mm
    /// gets its own XOL generation/token during fork-mm publication.
    needs_rebind: bool,
}

#[derive(Clone, Default)]
struct ThreadProbeState {
    pending_xol: Option<PendingXol>,
    /// Armed only while emulating a live optimized direct CALL.  The identity
    /// binds syscall 336 to that one task/mm/trampoline return and keeps an
    /// arbitrary jump into the public RX page as ENXIO.
    pending_uprobe_syscall: Option<PendingUprobeSyscall>,
    returns: Vec<ReturnInstance>,
}

/// A consumer which runs at a kernel-owned uprobe trampoline with the real
/// interrupted register frame.  It may redirect execution by changing RIP or
/// RSP and may change the syscall-clobbered register image.  This is kept
/// separate from perf sample delivery: samples observe a probe hit but are
/// not a substitute for Linux's uprobe consumer callback contract.
pub(crate) type UprobeFrameHandler = fn(&mut TrapFrame);

/// Invoke one registered in-kernel consumer.  The current perf/trace and BPF
/// attachment frontends are observers only; keeping this hook explicit makes
/// the trampoline ABI usable by consumers which need Linux's mutable pt_regs
/// semantics without granting any user controlled trampoline entry point.
fn handle_uprobe_trampoline(key: ProbeKey, retprobe: bool, frame: &mut TrapFrame) {
    let handlers = REGISTRY
        .lock()
        .consumers
        .get(&key)
        .map(|refs| {
            if retprobe {
                refs.return_handlers
            } else {
                refs.entry_handlers
            }
        })
        .unwrap_or([None; 8]);
    for handler in handlers.into_iter().flatten() {
        handler(frame);
    }
    // Keep the boolean in the ABI boundary: future consumers may select entry
    // versus return callbacks without exposing separate user trampolines.
    let _ = retprobe;
}

/// Add an in-kernel mutable-frame consumer to an already registered uprobe.
/// Registration is bounded and does not allocate on a #BP/trampoline path.
pub(crate) fn register_frame_handler(
    file: UprobeFileKey,
    offset: u64,
    retprobe: bool,
    handler: UprobeFrameHandler,
) -> AxResult<()> {
    let mut registry = REGISTRY.lock();
    let refs = registry
        .consumers
        .get_mut(&ProbeKey { file, offset })
        .ok_or(AxError::NotFound)?;
    let handlers = if retprobe {
        &mut refs.return_handlers
    } else {
        &mut refs.entry_handlers
    };
    if handlers
        .iter()
        .flatten()
        .any(|registered| *registered as usize == handler as usize)
    {
        return Ok(());
    }
    let slot = handlers
        .iter_mut()
        .find(|slot| slot.is_none())
        .ok_or(AxError::NoMemory)?;
    *slot = Some(handler);
    Ok(())
}

pub(crate) fn unregister_frame_handler(
    file: UprobeFileKey,
    offset: u64,
    retprobe: bool,
    handler: UprobeFrameHandler,
) {
    let mut registry = REGISTRY.lock();
    let Some(refs) = registry.consumers.get_mut(&ProbeKey { file, offset }) else {
        return;
    };
    let handlers = if retprobe {
        &mut refs.return_handlers
    } else {
        &mut refs.entry_handlers
    };
    for slot in handlers {
        if slot.is_some_and(|registered| registered as usize == handler as usize) {
            *slot = None;
        }
    }
}

/// Faults produced while emulating an instruction are architectural faults of
/// that instruction, not failures of the probe machinery.  In particular a
/// shadow-stack violation is #CP and must not be reduced to a page fault at
/// the probe site.
#[derive(Clone, Copy, Debug)]
enum ControlError {
    Memory(u64),
    ControlProtection,
    Unsupported,
}

fn shadow_stack_enabled() -> bool {
    axcpu::asm::user_shadow_stack_enabled()
        && crate::task::current_user_live_cet_state().u_cet & 1 != 0
}

fn signal_control_error(instruction_pointer: u64, error: ControlError) {
    let info = match error {
        // This is deliberately the effective address, including RSP for an
        // implicit push/pop, rather than the INT3 address.
        ControlError::Memory(address) => SignalInfo::new_fault(Signo::SIGSEGV, 1, address as usize),
        // Linux exposes #CP as SIGSEGV/SEGV_CPERR with si_addr = RIP because
        // #CP does not supply a CR2-style fault address.
        ControlError::ControlProtection => {
            SignalInfo::new_fault(Signo::SIGSEGV, SEGV_CPERR, instruction_pointer as usize)
        }
        ControlError::Unsupported => SignalInfo::new_kernel(Signo::SIGILL),
    };
    force_signal_current_thread(info);
}

/// A failed control-flow emulation is reported at the original instruction,
/// not at INT3+1 or a partially staged target.  `signal_control_error` keeps
/// its precise effective-address/#CP `si_addr` policy; this helper changes
/// only the restart PC exposed with that signal.
fn signal_probe_control_error(frame: &mut TrapFrame, probe_address: u64, error: ControlError) {
    frame.rip = probe_address;
    signal_control_error(probe_address, error);
}

/// Publish one entry hit from the architectural INT3 owner.  Control-flow
/// plans do not enter XOL, and return instances publish only from the return
/// trampoline, so keeping the entry publication here gives perf/BPF exactly
/// one observation per hit.
fn publish_entry_hit(frame: &TrapFrame, probe: InstalledProbe, task: u64) {
    let event = PerfEvent::Uprobe {
        mount_id: probe.key.file.mount_id,
        device: probe.key.file.device,
        inode: probe.key.file.inode,
        offset: probe.key.offset,
        retprobe: false,
        // Probe delivery is keyed by the instruction location.  Individual
        // perf descriptors retain their own USDT semaphore offset for query
        // and activation; it is not part of the hit identity.
        reference_counter_offset: 0,
    };
    let mut payload = [0u8; 48];
    payload[4..8].copy_from_slice(&(task as i32).to_ne_bytes());
    payload[8..16].copy_from_slice(&probe.address.to_ne_bytes());
    payload[16..24].copy_from_slice(&probe.key.file.mount_id.to_ne_bytes());
    payload[24..32].copy_from_slice(&probe.key.file.device.to_ne_bytes());
    payload[32..40].copy_from_slice(&probe.key.file.inode.to_ne_bytes());
    payload[40..48].copy_from_slice(&probe.key.offset.to_ne_bytes());
    crate::perf_sources::emit_current_raw_at(event, frame.rip, &payload);
}

fn publish_return_hit(instance: ReturnInstance, task: u64, active: bool) {
    if !active {
        return;
    }
    let event = PerfEvent::Uprobe {
        mount_id: instance.key.file.mount_id,
        device: instance.key.file.device,
        inode: instance.key.file.inode,
        offset: instance.key.offset,
        retprobe: true,
        reference_counter_offset: 0,
    };
    let mut payload = [0u8; 48];
    payload[4..8].copy_from_slice(&(task as i32).to_ne_bytes());
    payload[16..24].copy_from_slice(&instance.key.file.mount_id.to_ne_bytes());
    payload[24..32].copy_from_slice(&instance.key.file.device.to_ne_bytes());
    payload[32..40].copy_from_slice(&instance.key.file.inode.to_ne_bytes());
    payload[40..48].copy_from_slice(&instance.key.offset.to_ne_bytes());
    crate::perf_sources::emit_current_raw(event, &payload);
}

/// Linux's internal trampoline syscalls authorize the precise post-syscall
/// instruction pointer in the mm-owned special RX mapping, not a merely
/// numerically matching address left behind by a torn VMA transaction.
fn is_live_trampoline_syscall_ip(mm_id: u64, rip: u64, offset: u64) -> bool {
    let identity = REGISTRY
        .lock()
        .mms
        .get(&mm_id)
        .map(|state| (state.trampoline_base, state.xol_base, state.xol_token));
    let Some((trampoline, xol_base, token)) = identity else {
        return false;
    };
    if rip != trampoline.wrapping_add(offset) || token == 0 {
        return false;
    }
    let aspace = current_aspace();
    let mm = aspace.lock();
    is_exact_xol_vma(&mm, xol_base, token)
}

/// The #BP owner is explicit: only `Unowned` may reach normal user SIGTRAP
/// delivery.  This prevents a stale or malformed XOL transition from being
/// silently reclassified as an application breakpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BreakpointClaim {
    Unowned,
    Claimed,
}

fn claim_xol_failure(frame: &mut TrapFrame, address: u64) -> BreakpointClaim {
    // An installed INT3 is kernel-owned even when this first-stage XOL path
    // cannot service it. Never relabel it as the program's SIGTRAP.  The
    // instruction is not executed at a synthetic PC; SIGILL makes the
    // incomplete relocation visible without exposing the probe byte itself.
    frame.rip = address;
    force_signal_current_thread(SignalInfo::new_kernel(Signo::SIGILL));
    BreakpointClaim::Claimed
}

/// Thread state is keyed by the scheduler's non-reused task identity.  It is
/// retired at exit and cleared at exec; no record can outlive its task.
static THREADS: Mutex<BTreeMap<u64, ThreadProbeState>> = Mutex::new(BTreeMap::new());

fn current_mm_id() -> u64 {
    axtask::current()
        .as_thread()
        .proc_data
        .aspace()
        .lock()
        .address_space_id()
        .get()
}

fn current_task_id() -> u64 {
    axtask::current().as_thread().kernel_tid() as u64
}

fn current_aspace() -> Arc<Mutex<AddrSpace>> {
    axtask::current().as_thread().proc_data.aspace()
}

fn is_exact_xol_vma(mm: &AddrSpace, base: u64, token: u64) -> bool {
    if base == 0 || token == 0 {
        return false;
    }
    mm.find_area(VirtAddr::from(base as usize))
        .is_some_and(|area| {
            area.start().as_usize() as u64 == base
                && area.end().as_usize() as u64 == base.saturating_add(PAGE_SIZE_4K as u64)
                && area.flags() == (MappingFlags::USER | MappingFlags::READ | MappingFlags::EXECUTE)
                && area.backend().special_mapping_token() == Some(token)
        })
}

fn patch_overlay(aspace: &Arc<Mutex<AddrSpace>>, address: u64, byte: u8) -> AxResult<u8> {
    crate::file::executable::patch_private_executable_alias(
        aspace,
        VirtAddr::from(address as usize),
        byte,
    )
}

fn preflight_instruction_plan(
    aspace: &Arc<Mutex<AddrSpace>>,
    address: u64,
    original_byte: u8,
) -> AxResult<InstructionPlan> {
    let memory = UserMemoryCapability::new(aspace.clone());
    let mut raw = [MaybeUninit::<u8>::uninit(); 15];
    let readable = (PAGE_SIZE_4K - (address as usize & (PAGE_SIZE_4K - 1))).min(15);
    memory
        .read_bytes(address as usize, &mut raw[..readable])
        .map_err(map_usercopy_error)?;
    let mut bytes = [0u8; 15];
    for (dst, src) in bytes.iter_mut().zip(raw[..readable].iter()) {
        *dst = unsafe { src.assume_init() };
    }
    plan_from_bytes(address, original_byte, bytes, readable)
}

fn plan_from_bytes(
    address: u64,
    original_byte: u8,
    mut bytes: [u8; 15],
    readable: usize,
) -> AxResult<InstructionPlan> {
    bytes[0] = original_byte;
    let mut decoder = Decoder::with_ip(64, &bytes[..readable], address, DecoderOptions::NONE);
    let instruction = decoder.decode();
    if instruction.is_invalid() || instruction.len() == 0 {
        return Ok(InstructionPlan::ControlledUnsupported {
            bytes,
            len: readable as u8,
        });
    }
    // BlockEncoder owns RIP-relative displacement and ordinary relative
    // immediate post-fixups when moving a straight-line instruction to XOL.
    // PUSH/flags instructions remain on this path: executing them once at
    // XOL is architecturally equivalent. Only control transfers require a
    // separate stack/CET-aware continuation engine.
    if matches!(
        instruction.mnemonic(),
        Mnemonic::Loop
            | Mnemonic::Loope
            | Mnemonic::Loopne
            | Mnemonic::Jcxz
            | Mnemonic::Jecxz
            | Mnemonic::Jrcxz
    ) {
        return Ok(InstructionPlan::LoopControl {
            mnemonic: instruction.mnemonic(),
            target: instruction.near_branch_target(),
            len: instruction.len() as u8,
        });
    }
    let direct_near = instruction.op0_kind() == OpKind::NearBranch64;
    let near_return = matches!(instruction.code(), Code::Retnq | Code::Retnq_imm16);
    if (matches!(
        instruction.flow_control(),
        FlowControl::Call | FlowControl::UnconditionalBranch | FlowControl::ConditionalBranch
    ) && direct_near)
        || (instruction.flow_control() == FlowControl::Return && near_return)
    {
        return Ok(InstructionPlan::DirectControl {
            flow: instruction.flow_control(),
            target: instruction.near_branch_target(),
            condition: instruction.condition_code(),
            len: instruction.len() as u8,
            stack_increment: u32::try_from(instruction.stack_pointer_increment())
                .map_err(|_| AxError::Unsupported)?,
        });
    }
    if instruction.flow_control() != FlowControl::Next {
        return Ok(InstructionPlan::ControlledUnsupported {
            bytes,
            len: instruction.len() as u8,
        });
    }
    Ok(InstructionPlan::Relocated {
        bytes,
        len: instruction.len() as u8,
    })
}

/// Restores one final-consumer overlay while serializing the decision against
/// a concurrent registration of the same object+offset.  MM mutation paths
/// already use the `mm -> REGISTRY` lock order; retaining that order here
/// means a re-register either wins before this check (and keeps INT3) or after
/// the original byte and registry record have both been retired (and performs
/// a fresh installation).
fn restore_if_inactive(mm_id: u64, mm: &mut AddrSpace, probe: InstalledProbe) -> AxResult<()> {
    let mut registry = REGISTRY.lock();
    if registry
        .consumers
        .get(&probe.key)
        .is_some_and(|refs| refs.total != 0)
    {
        return Ok(());
    }
    let still_owned = registry
        .mms
        .get(&mm_id)
        .and_then(|state| state.installed.get(&probe.address))
        .is_some_and(|installed| installed.key == probe.key);
    if !still_owned {
        return Ok(());
    }
    let mapped_to_same_probe = mm
        .find_area(VirtAddr::from(probe.address as usize))
        .and_then(|area| {
            let mapping = area.backend().file_mapping()?;
            let identity = mapping.identity();
            let file = UprobeFileKey {
                mount_id: identity.mount_id(),
                device: identity.device(),
                inode: identity.inode(),
            };
            Some(
                mapping.sharing() == crate::mm::FileMappingSharing::Private
                    && file == probe.key.file
                    && mapping.file_offset_at(VirtAddr::from(probe.address as usize))
                        == Some(probe.key.offset),
            )
        })
        .unwrap_or(false);
    if mapped_to_same_probe {
        mm.uprobe_cow_patch_byte(VirtAddr::from(probe.address as usize), probe.original_byte)?;
    }
    if let Some(state) = registry.mms.get_mut(&mm_id) {
        state.installed.remove(&probe.address);
    }
    Ok(())
}

/// Allocation-free counterpart used by the thread which has already trapped
/// on a retiring INT3.  The fetched leaf is resident, so failing this exact
/// restore is an internal page-table/overlay invariant violation rather than
/// a userspace breakpoint condition.
fn restore_trapped_if_inactive(
    mm_id: u64,
    aspace: &Arc<Mutex<AddrSpace>>,
    probe: InstalledProbe,
) -> AxResult<bool> {
    let mut mm = aspace.lock();
    let mut registry = REGISTRY.lock();
    if registry
        .consumers
        .get(&probe.key)
        .is_some_and(|refs| refs.total != 0)
    {
        return Ok(true);
    }
    let still_owned = registry
        .mms
        .get(&mm_id)
        .and_then(|state| state.installed.get(&probe.address))
        .is_some_and(|installed| installed.key == probe.key);
    if !still_owned {
        return Ok(true);
    }
    let mapped_to_same_probe = mm
        .find_area(VirtAddr::from(probe.address as usize))
        .and_then(|area| {
            let mapping = area.backend().file_mapping()?;
            let identity = mapping.identity();
            Some(
                mapping.sharing() == crate::mm::FileMappingSharing::Private
                    && identity.mount_id() == probe.key.file.mount_id
                    && identity.device() == probe.key.file.device
                    && identity.inode() == probe.key.file.inode
                    && mapping.file_offset_at(VirtAddr::from(probe.address as usize))
                        == Some(probe.key.offset),
            )
        })
        .unwrap_or(false);
    if !mapped_to_same_probe {
        // The prior VMA disappeared with its private overlay.  Forget only
        // the stale ownership record and let the replacement mapping's INT3
        // continue to the ordinary debugger/SIGTRAP path.
        if let Some(state) = registry.mms.get_mut(&mm_id) {
            state.installed.remove(&probe.address);
        }
        return Ok(false);
    }
    mm.uprobe_restore_trapped_byte(VirtAddr::from(probe.address as usize), probe.original_byte)?;
    if let Some(state) = registry.mms.get_mut(&mm_id) {
        state.installed.remove(&probe.address);
    }
    Ok(true)
}

/// Whether the dedicated task-context owner has a last-consumer byte restore
/// to retry.  The registry entry itself is the custody record, so enqueueing
/// never allocates and no detached work item can outlive its mm weak lease.
pub(crate) fn has_deferred_restore_work() -> bool {
    if crate::perf_sources::has_deferred_kprobe_restore_work() {
        return true;
    }
    let registry = REGISTRY.lock();
    registry.consumers.iter().any(|(key, refs)| {
        refs.retiring
            && refs.total == 0
            && registry.mms.values().all(|state| {
                state.installed.values().all(|probe| probe.key != *key)
                    && state
                        .reference_counters
                        .keys()
                        .all(|(applied_key, _)| applied_key != key)
            })
    }) || registry.mms.iter().any(|(mm_id, state)| {
        state.pending_mmap_reconcile
            || state.installed.values().any(|probe| {
                registry
                    .consumers
                    .get(&probe.key)
                    .is_none_or(|refs| refs.total == 0)
            })
            || state
                .reference_counters
                .iter()
                .any(|((key, offset), applied)| {
                    !counter_is_blocked(state, &(*key, *offset))
                        && desired_reference_counter_for_mm(&registry, *mm_id, *key, *offset)
                            != applied.count
                })
    })
}

/// Attempts one pending restoration.  A transient COW/populate failure keeps
/// the exact registry custody record for the sleeping worker's next retry.
pub(crate) fn drain_one_deferred_restore() -> bool {
    if crate::perf_sources::has_deferred_kprobe_restore_work() {
        return crate::perf_sources::drain_one_deferred_kprobe_restore();
    }
    let pending_mmap = {
        let registry = REGISTRY.lock();
        registry.mms.iter().find_map(|(mm_id, state)| {
            state
                .pending_mmap_reconcile
                .then_some((*mm_id, state.aspace.clone()))
        })
    };
    if let Some((mm_id, weak)) = pending_mmap {
        let Some(aspace) = weak.upgrade() else {
            REGISTRY.lock().mms.remove(&mm_id);
            return true;
        };
        // Match every topology edge's ordering: global registration gate,
        // then mm lock. A retry is best effort too; retain the bit on failure.
        let _topology = registration_topology_gate();
        let mut mm = aspace.lock();
        if let Some(state) = REGISTRY.lock().mms.get_mut(&mm_id) {
            state.pending_mmap_reconcile = false;
        }
        if reconcile_mm_locked_gated(&aspace, &mut mm).is_err() {
            if let Some(state) = REGISTRY.lock().mms.get_mut(&mm_id) {
                state.pending_mmap_reconcile = true;
            }
            // Returning false asks the existing deferred-work scheduler to
            // apply its bounded backoff. Do not self-wake into a hot retry.
            return false;
        } else if let Some(state) = REGISTRY.lock().mms.get_mut(&mm_id) {
            state
                .reference_counters
                .retain(|_, applied| applied.count != 0);
            state
                .blocked_reference_counters
                .retain(|_, state| *state != CounterBlockState::Reserved);
        }
        return true;
    }
    let pending_counter = {
        let registry = REGISTRY.lock();
        registry.mms.iter().find_map(|(mm_id, state)| {
            state
                .reference_counters
                .iter()
                .find_map(|((key, offset), applied)| {
                    (!counter_is_blocked(state, &(*key, *offset))
                        && desired_reference_counter_for_mm(&registry, *mm_id, *key, *offset)
                            != applied.count)
                        .then_some((*mm_id, state.aspace.clone(), *key, *offset))
                })
        })
    };
    if let Some((mm_id, weak, key, offset)) = pending_counter {
        let Some(aspace) = weak.upgrade() else {
            REGISTRY.lock().mms.remove(&mm_id);
            return true;
        };
        return sync_reference_counter(&aspace, key, offset).is_ok();
    }
    let pending = {
        let registry = REGISTRY.lock();
        registry.mms.iter().find_map(|(mm_id, state)| {
            state
                .installed
                .values()
                .find(|probe| {
                    registry
                        .consumers
                        .get(&probe.key)
                        .is_none_or(|refs| refs.total == 0)
                })
                .copied()
                .map(|probe| (*mm_id, state.aspace.clone(), probe))
        })
    };
    let Some((mm_id, weak, probe)) = pending else {
        let mut registry = REGISTRY.lock();
        let retired = registry.consumers.iter().find_map(|(key, refs)| {
            (refs.retiring
                && refs.total == 0
                && registry.mms.values().all(|state| {
                    state
                        .reference_counters
                        .keys()
                        .all(|(applied_key, _)| applied_key != key)
                }))
            .then_some(*key)
        });
        if let Some(key) = retired {
            registry.consumers.remove(&key);
            return true;
        }
        return false;
    };
    let Some(aspace) = weak.upgrade() else {
        REGISTRY.lock().mms.remove(&mm_id);
        return true;
    };
    let mut mm = aspace.lock();
    restore_if_inactive(mm_id, &mut mm, probe).is_ok()
}

fn prepare_return_instance(
    frame: &mut TrapFrame,
    probe: InstalledProbe,
) -> Result<bool, ControlError> {
    if !probe.retprobe {
        return Ok(false);
    }
    let mm_id = current_mm_id();
    let trampoline = REGISTRY
        .lock()
        .mms
        .get(&mm_id)
        .map(|mm| mm.trampoline_base)
        .filter(|base| *base != 0)
        .ok_or(ControlError::Unsupported)?;
    let task = current_task_id();
    // Reserve all thread-state storage before the architectural transaction.
    // The post-copy publication below must not allocate or otherwise fail.
    {
        let mut threads = THREADS.lock();
        let state = threads.entry(task).or_default();
        if state.returns.len() >= MAX_RETURN_INSTANCES || state.returns.try_reserve(1).is_err() {
            return Err(ControlError::Memory(frame.rsp));
        }
    }
    // Validate both stacks before changing either.  The live SSP names the
    // end of its current top word in this kernel's CET model; replace that
    // word without moving SSP, matching Linux's update-last-frame operation.
    let shadow_stack = if shadow_stack_enabled() {
        let state = crate::task::current_user_live_cet_state();
        let next = state
            .pl3_ssp
            .checked_sub(8)
            .ok_or(ControlError::ControlProtection)?;
        Some(next)
    } else {
        None
    };
    let replacement_previous = commit_return_words(frame.rsp, trampoline, shadow_stack)?;
    // A tail call (and some compiler generated sibling-call sequences) can
    // reach another retprobe after the return slot already belongs to this
    // trampoline.  Linux chains those instances instead of silently dropping
    // the later consumer.  The first instance owns the real return address;
    // each later one only adds another return handler at the same stack slot.
    let chained = replacement_previous == trampoline;
    let original = if chained {
        THREADS
            .lock()
            .get(&task)
            .and_then(|state| {
                state
                    .returns
                    .iter()
                    .rev()
                    .find(|instance| {
                        instance.stack == frame.rsp && instance.trampoline == trampoline
                    })
                    .map(|instance| instance.original_return)
            })
            .ok_or(ControlError::Memory(frame.rsp))?
    } else {
        replacement_previous
    };
    let mut threads = THREADS.lock();
    let state = threads
        .get_mut(&task)
        .expect("uprobe return storage was reserved");
    state.returns.push(ReturnInstance {
        original_return: original,
        function: probe.address,
        key: probe.key,
        stack: frame.rsp,
        shadow_stack,
        trampoline,
        owns_trampoline: !chained,
        chained,
    });
    Ok(true)
}

/// Undo the just-installed return trampoline before reporting a fault from
/// the trapped instruction.  The record stays live until both architectural
/// stacks have been restored, so a failed rollback cannot manufacture an
/// unowned return through a stale trampoline.
fn rollback_return_instance(probe: InstalledProbe, prepared: bool) -> Result<(), ControlError> {
    if !prepared {
        return Ok(());
    }
    let task = current_task_id();
    let instance = THREADS
        .lock()
        .get(&task)
        .and_then(|state| state.returns.last().copied())
        .filter(|instance| instance.key == probe.key && instance.function == probe.address)
        .ok_or(ControlError::Memory(0))?;
    if instance.owns_trampoline {
        restore_return_words(instance)?;
    } else {
        debug_assert!(instance.chained);
    }
    let mut threads = THREADS.lock();
    let state = threads
        .get_mut(&task)
        .ok_or(ControlError::Memory(instance.stack))?;
    if state.returns.last().is_some_and(|last| {
        last.key == instance.key
            && last.function == instance.function
            && last.stack == instance.stack
    }) {
        state.returns.pop();
        Ok(())
    } else {
        Err(ControlError::Memory(instance.stack))
    }
}

/// Restore an already-published return instance without a compensating write
/// path.  Both live trampoline words are checked and pinned before either
/// original word is made visible, so an interrupted rollback leaves the
/// original instance structurally intact instead of half-restored.
fn restore_return_words(instance: ReturnInstance) -> Result<(), ControlError> {
    let aspace = current_aspace();
    let result = try_user_nofault_transaction(&aspace, |tx| {
        let normal_read = tx.pin_read(instance.stack as usize, 8)?;
        let normal_write = tx.pin_user_write(instance.stack as usize, 8)?;
        let mut normal = [0u8; 8];
        tx.read_pinned(&normal_read, &mut normal);
        if u64::from_ne_bytes(normal) != instance.trampoline {
            return Ok(Err(ControlError::Memory(instance.stack)));
        }
        let shadow_write = if let Some(shadow) = instance.shadow_stack {
            let shadow_read = match tx.pin_read(shadow as usize, 8) {
                Ok(span) => span,
                Err(_) => return Ok(Err(ControlError::ControlProtection)),
            };
            let shadow_write = match tx.pin_shadow_stack_write(shadow as usize, 8) {
                Ok(span) => span,
                Err(_) => return Ok(Err(ControlError::ControlProtection)),
            };
            let mut shadow_word = [0u8; 8];
            tx.read_pinned(&shadow_read, &mut shadow_word);
            if u64::from_ne_bytes(shadow_word) != instance.trampoline {
                return Ok(Err(ControlError::ControlProtection));
            }
            Some(shadow_write)
        } else {
            None
        };
        tx.write_pinned(&normal_write, &instance.original_return.to_ne_bytes());
        if let Some(shadow_write) = shadow_write {
            tx.write_pinned(&shadow_write, &instance.original_return.to_ne_bytes());
        }
        Ok(Ok(()))
    });
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(ControlError::Memory(instance.stack)),
    }
}

const XOL_TRAMPOLINE_OFFSET: usize = 0;
const UPROBE_TRAMPOLINE_OFFSET: usize = 64;
const XOL_SLOT_OFFSET: usize = 128;
const XOL_SLOT_SIZE: usize = 128;
const XOL_SLOT_COUNT: usize = (PAGE_SIZE_4K - XOL_SLOT_OFFSET) / XOL_SLOT_SIZE;
// `uretprobe`: push rax,rcx,r11; mov $__NR_uretprobe,%rax; syscall;
// pop r11,rcx; ret; int3. The mapping is RX from publication.
const URETPROBE_TRAMPOLINE: &[u8] = &[
    0x50, 0x51, 0x41, 0x53, 0x48, 0xc7, 0xc0, 0x4f, 0x01, 0x00, 0x00, 0x0f, 0x05, 0x41, 0x5b, 0x59,
    0xc3, 0xcc,
];
// `uprobe`: push rcx,r11,rax; mov $__NR_uprobe,%rax; syscall;
// pop rax,r11,rcx; ret; int3.
const UPROBE_TRAMPOLINE: &[u8] = &[
    0x51, 0x41, 0x53, 0x50, 0x48, 0xc7, 0xc0, 0x50, 0x01, 0x00, 0x00, 0x0f, 0x05, 0x58, 0x41, 0x5b,
    0x59, 0xc3, 0xcc,
];

/// Build the per-mm special trampoline page.  Its VMA is executable and
/// readable from the outset; bytes are populated through the direct kernel
/// alias, never by temporarily granting user write permission.
fn ensure_xol_mapping_locked(aspace: &Arc<Mutex<AddrSpace>>, mm: &mut AddrSpace) -> AxResult<u64> {
    ensure_xol_mapping_locked_avoiding(aspace, mm, None)
}

/// Variant used by an in-flight MAP_FIXED replacement.  The destination may
/// still be an unmapped hole while XOL is prepared; never allocate the
/// trampoline in that hole and let the replacement overwrite it.
fn ensure_xol_mapping_locked_avoiding(
    aspace: &Arc<Mutex<AddrSpace>>,
    mm: &mut AddrSpace,
    excluded: Option<(VirtAddr, usize)>,
) -> AxResult<u64> {
    let mm_id = mm.address_space_id().get();
    let mut registry = REGISTRY.lock();
    if let Some(state) = registry.mms.get_mut(&mm_id) {
        if is_exact_xol_vma(mm, state.xol_base, state.xol_token) {
            return Ok(state.xol_base);
        }
        // A prior destructive VMA transaction may already have removed the
        // special page. Retire this identity before selecting a new address;
        // no ordinary VMA can inherit its token.
        state.xol_base = 0;
        state.trampoline_base = 0;
        state.xol_generation = 0;
        state.xol_token = 0;
        state.xol_slots.clear();
    }
    drop(registry);
    let token = NEXT_XOL_TOKEN.fetch_add(1, Ordering::Relaxed);
    let full_limit = VirtAddrRange::new(mm.base(), mm.end());
    let base = if let Some((excluded_start, excluded_size)) = excluded {
        let excluded_end = excluded_start
            .checked_add(excluded_size)
            .ok_or(AxError::InvalidInput)?;
        // Prefer above the replacement, then use the disjoint lower interval.
        // Both searches have a limit which excludes the fixed destination by
        // construction, not by a racy post-selection check.
        let above = (excluded_end < mm.end())
            .then(|| {
                mm.find_free_area_avoiding_shadow_stack_guards(
                    excluded_end,
                    PAGE_SIZE_4K,
                    VirtAddrRange::new(excluded_end, mm.end()),
                    PAGE_SIZE_4K,
                )
            })
            .flatten();
        above.or_else(|| {
            (mm.base() < excluded_start)
                .then(|| {
                    mm.find_free_area_avoiding_shadow_stack_guards(
                        mm.base(),
                        PAGE_SIZE_4K,
                        VirtAddrRange::new(mm.base(), excluded_start),
                        PAGE_SIZE_4K,
                    )
                })
                .flatten()
        })
    } else {
        mm.find_free_area_avoiding_shadow_stack_guards(
            mm.base(),
            PAGE_SIZE_4K,
            full_limit,
            PAGE_SIZE_4K,
        )
    }
    .ok_or(AxError::NoMemory)?;
    let mut backend = Backend::new_alloc(base, PageSize::Size4K);
    backend.set_special_mapping_token(token);
    mm.map(
        base,
        PAGE_SIZE_4K,
        MappingFlags::USER | MappingFlags::READ | MappingFlags::EXECUTE,
        true,
        backend,
    )?;
    if let Err(error) = (|| {
        mm.write(base + XOL_TRAMPOLINE_OFFSET, URETPROBE_TRAMPOLINE)?;
        mm.write(base + UPROBE_TRAMPOLINE_OFFSET, UPROBE_TRAMPOLINE)?;
        Ok(())
    })() {
        // No registry identity has been published yet. Leaving this RX VMA
        // behind would make a later ordinary mapping inherit an untracked
        // trampoline address, so construction is all-or-nothing. A cleanup
        // failure is a fail-stop MM contract violation: continuing would be
        // less safe than surfacing a stray executable special mapping.
        let wake = mm
            .unmap(base, PAGE_SIZE_4K)
            .expect("failed XOL construction must unmap its unpublished VMA");
        wake.finish();
        return Err(error);
    }
    let base = base.as_usize() as u64;
    let generation = NEXT_OVERLAY_GENERATION.fetch_add(1, Ordering::Relaxed);
    let mut registry = REGISTRY.lock();
    let state = registry.mms.entry(mm_id).or_default();
    state.xol_base = base;
    state.trampoline_base = base;
    state.xol_generation = generation;
    state.xol_token = token;
    state.aspace = Arc::downgrade(aspace);
    Ok(base)
}

fn ensure_xol_mapping(aspace: &Arc<Mutex<AddrSpace>>) -> AxResult<u64> {
    let mut mm = aspace.lock();
    ensure_xol_mapping_locked(aspace, &mut mm)
}

/// Called while the mmap transaction owns the mm lock, before MAP_FIXED tears
/// down an overlapping range.  The XOL identity must die before a user VMA
/// can recycle its virtual address; otherwise a cached base would turn an
/// ordinary executable mapping into an XOL code page.
pub(crate) fn invalidate_xol_range_locked(aspace: &AddrSpace, start: VirtAddr, length: usize) {
    let end = start.as_usize().saturating_add(length) as u64;
    let mm_id = aspace.address_space_id().get();
    let mut registry = REGISTRY.lock();
    let Some(state) = registry.mms.get_mut(&mm_id) else {
        return;
    };
    let xol_end = state.xol_base.saturating_add(PAGE_SIZE_4K as u64);
    if state.xol_base != 0 && (start.as_usize() as u64) < xol_end && end > state.xol_base {
        state.xol_base = 0;
        state.trampoline_base = 0;
        state.xol_generation = 0;
        state.xol_token = 0;
        state.xol_slots.clear();
    }
}

/// Records a post-publication uprobe retry without changing mmap/munmap's
/// success result. Callers hold the topology gate and mm lock.
fn defer_mmap_reconcile_locked(aspace: &Arc<Mutex<AddrSpace>>, mm_id: u64) {
    let mut registry = REGISTRY.lock();
    let state = registry.mms.entry(mm_id).or_default();
    state.aspace = Arc::downgrade(aspace);
    state.pending_mmap_reconcile = true;
    drop(registry);
    crate::deferred_work::wake_uprobe_restore_worker();
}

/// One USDT write which may outlive the replaced VMA.  A semaphore is often
/// mapped through a separate writable alias of the same file, so restoring
/// the fixed-replacement PTE journal alone is insufficient on abort.
#[derive(Clone, Copy)]
struct FixedReplacementCounterJournal {
    address: u64,
    bytes: [u8; 2],
}

/// All allocation-bearing registry discovery for one incoming executable
/// mapping.  Decoding is deliberately deferred until the incoming bytes are
/// mapped, but the consumer key, address and destination custody slot are
/// fixed while the old topology is still live.
#[derive(Clone, Copy)]
struct PreparedFixedProbe {
    address: u64,
    key: ProbeKey,
    retprobe: bool,
}

/// Uprobe's participant in an atomic MAP_FIXED replacement.
///
/// Construction happens while the old mapping is still live and while the
/// caller owns `REGISTRATION_GATE` followed by the mm lock.  That order is
/// important: a register/unregister path cannot publish a consumer between
/// the snapshot and the replacement's commit/rollback edge.  Commit runs
/// after the incoming VMA/PTEs are installed but before the old retirement is
/// released; rollback runs under those same gates before the memory-set guard
/// restores the old leaves.
pub(crate) struct PreparedFixedUprobeTransition {
    aspace: Arc<Mutex<AddrSpace>>,
    mm_id: u64,
    start: VirtAddr,
    length: usize,
    /// Exact old registry/XOL authority.  It is moved back as one object on
    /// rollback, rather than trying to reconstruct BTree nodes after an
    /// allocation failure in the incoming installer.
    old_state: Option<MmProbeState>,
    /// Every counter identity which either the retiring records or the
    /// incoming private executable mapping can touch.  This is reserved in
    /// prepare; commit only fills it and never discovers a new identity.
    counter_identities: Vec<(ProbeKey, u64)>,
    incoming_probes: Vec<PreparedFixedProbe>,
    counter_journal: Vec<FixedReplacementCounterJournal>,
    /// Exact incoming INT3 bytes.  The vector capacity is based on the
    /// prepared consumer set, so rollback does not allocate while the MM is
    /// still in its provisional topology.
    patched: Vec<(u64, u8)>,
    created_xol_base: Option<u64>,
    /// Incoming executable probes may be published only after an exact XOL
    /// trampoline identity exists for this mm.
    instrumentation_ready: bool,
}

impl PreparedFixedUprobeTransition {
    /// Linux calls `uprobe_mmap()` only after VMA publication and discards its
    /// error.  Fixed replacement needs a participant even when preallocation
    /// fails, so this wrapper degrades to a deferred-only participant instead
    /// of turning an otherwise valid mmap/shmat into ENOMEM.
    pub(crate) fn prepare_or_defer_locked(
        aspace_handle: &Arc<Mutex<AddrSpace>>,
        aspace: &AddrSpace,
        start: VirtAddr,
        length: usize,
        incoming: &Backend,
        incoming_flags: MappingFlags,
    ) -> Self {
        match Self::prepare_locked(
            aspace_handle,
            aspace,
            start,
            length,
            incoming,
            incoming_flags,
        ) {
            Ok(prepared) => prepared,
            Err(_) => Self::deferred_only(aspace_handle, aspace, start, length),
        }
    }

    fn deferred_only(
        aspace_handle: &Arc<Mutex<AddrSpace>>,
        aspace: &AddrSpace,
        start: VirtAddr,
        length: usize,
    ) -> Self {
        let mm_id = aspace.address_space_id().get();
        let mut registry = REGISTRY.lock();
        let old_state = registry.mms.get(&mm_id).cloned();
        let state = registry.mms.entry(mm_id).or_default();
        state.aspace = Arc::downgrade(aspace_handle);
        state.pending_mmap_reconcile = true;
        drop(registry);
        crate::deferred_work::wake_uprobe_restore_worker();
        Self {
            aspace: aspace_handle.clone(),
            mm_id,
            start,
            length,
            old_state,
            counter_identities: Vec::new(),
            incoming_probes: Vec::new(),
            counter_journal: Vec::new(),
            patched: Vec::new(),
            created_xol_base: None,
            instrumentation_ready: false,
        }
    }

    /// Prepare uprobe custody for a MAP_FIXED-like mapping replacement.
    ///
    /// The caller must already hold `registration_topology_gate()` and the
    /// address-space mutex.  `incoming` is inspected while still detached so
    /// all possible incoming consumer/counter identities are known before
    /// the old VMA is retired.
    pub(crate) fn prepare_locked(
        aspace_handle: &Arc<Mutex<AddrSpace>>,
        aspace: &AddrSpace,
        start: VirtAddr,
        length: usize,
        incoming: &Backend,
        incoming_flags: MappingFlags,
    ) -> AxResult<Self> {
        let mm_id = aspace.address_space_id().get();
        let end = start.checked_add(length).ok_or(AxError::InvalidInput)?;
        let registry = REGISTRY.lock();
        let old_state = registry.mms.get(&mm_id).cloned();

        // Bound every fallible Vec allocation before the VMA/PTE commit.  A
        // key can contribute at most one declared semaphore offset today;
        // retain the general vector representation so future multi-counter
        // consumers remain covered without changing this transaction.
        let consumer_bound = registry.consumers.len();
        let consumer_counter_bound = registry
            .consumers
            .values()
            .map(|refs| refs.reference_counters.len())
            .sum::<usize>();
        let applied_bound = old_state
            .as_ref()
            .map_or(0, |state| state.reference_counters.len());
        let mut counter_identities = Vec::new();
        counter_identities
            .try_reserve(
                consumer_bound
                    .saturating_add(consumer_counter_bound)
                    .saturating_add(applied_bound),
            )
            .map_err(|_| AxError::NoMemory)?;

        if let Some(state) = &old_state {
            for identity in state.reference_counters.keys().copied() {
                if !counter_identities.contains(&identity) {
                    debug_assert!(counter_identities.len() < counter_identities.capacity());
                    counter_identities.push(identity);
                }
            }
            for probe in state.installed.values() {
                let address = VirtAddr::from(probe.address as usize);
                if address >= start && address < end {
                    add_active_counter_identities(&registry, probe.key, &mut counter_identities);
                }
            }
        }
        let mut incoming_probes = Vec::new();
        incoming_probes
            .try_reserve_exact(consumer_bound)
            .map_err(|_| AxError::NoMemory)?;
        if incoming_flags.contains(MappingFlags::EXECUTE)
            && incoming.is_private_cow()
            && let Some(mapping) = incoming.file_mapping()
            && mapping.sharing() == crate::mm::FileMappingSharing::Private
            && let Some(first) = mapping.file_offset_at(start)
        {
            let identity = mapping.identity();
            let file = UprobeFileKey {
                mount_id: identity.mount_id(),
                device: identity.device(),
                inode: identity.inode(),
            };
            let last = first.saturating_add(length as u64);
            for key in registry.consumers.keys().copied() {
                if key.file == file && key.offset >= first && key.offset < last {
                    add_active_counter_identities(&registry, key, &mut counter_identities);
                    let retprobe = registry
                        .consumers
                        .get(&key)
                        .is_some_and(|refs| refs.returns != 0);
                    incoming_probes.push(PreparedFixedProbe {
                        address: start.as_usize() as u64 + (key.offset - first),
                        key,
                        retprobe,
                    });
                }
            }
        }
        drop(registry);

        let mut counter_journal = Vec::new();
        counter_journal
            .try_reserve_exact(counter_identities.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut patched = Vec::new();
        patched
            .try_reserve_exact(incoming_probes.len())
            .map_err(|_| AxError::NoMemory)?;

        // Stage registry custody off to the side.  `prepare_locked` must be
        // failure-atomic: a Vec/table admission error cannot leave an empty
        // mm registry entry, a changed weak identity, or a partially COWed
        // external semaphore alias behind.
        let mut staged_state = old_state.clone().unwrap_or_default();
        staged_state.aspace = Arc::downgrade(aspace_handle);
        staged_state
            .installed
            .try_reserve_remapped(incoming_probes.len())?;
        for identity in counter_identities.iter().copied() {
            staged_state
                .reference_counters
                .entry(identity)
                .or_insert(AppliedReferenceCounter {
                    address: 0,
                    count: 0,
                });
            staged_state
                .blocked_reference_counters
                .entry(identity)
                .or_insert(CounterBlockState::Reserved);
        }

        // Every fallible preparation above completed without touching the
        // live registry or any PTE. BTreeMap's entry insertion is the single
        // final publication of this detached state while the old VMA is live.
        let mut registry = REGISTRY.lock();
        registry.mms.insert(mm_id, staged_state);
        drop(registry);
        let instrumentation_ready = incoming_probes.is_empty();
        Ok(Self {
            aspace: aspace_handle.clone(),
            mm_id,
            start,
            length,
            old_state,
            counter_identities,
            incoming_probes,
            counter_journal,
            patched,
            created_xol_base: None,
            instrumentation_ready,
        })
    }

    fn overlaps_old_xol(&self) -> bool {
        let Some(state) = self.old_state.as_ref() else {
            return false;
        };
        let end = self.start.as_usize().saturating_add(self.length) as u64;
        let xol_end = state.xol_base.saturating_add(PAGE_SIZE_4K as u64);
        state.xol_base != 0 && (self.start.as_usize() as u64) < xol_end && end > state.xol_base
    }

    fn defer_mmap_reconcile(&self) {
        if let Some(state) = REGISTRY.lock().mms.get_mut(&self.mm_id) {
            state.pending_mmap_reconcile = true;
        }
        crate::deferred_work::wake_uprobe_restore_worker();
    }

    /// Capture every writable counter selected by the *new* VMA topology
    /// before it can be changed.  A duplicate address is journaled once even
    /// when several probes share one USDT semaphore.
    fn journal_counter_bytes(&mut self, aspace: &mut AddrSpace) -> AxResult<()> {
        for (key, offset) in self.counter_identities.iter().copied() {
            let ReferenceCounterMapping::Writable(address) =
                mapped_reference_counter(aspace, key.file, offset)
            else {
                continue;
            };
            if self
                .counter_journal
                .iter()
                .any(|entry| entry.address == address)
            {
                continue;
            }
            // The selected writable alias may be lazy. Materialize it before
            // sampling the exact two bytes; otherwise a later sync could
            // fault/COW first and our rollback journal would describe no
            // concrete incoming leaf. The replacement MM guard owns target
            // leaves, while external aliases are recorded below before their
            // user-visible counter write.
            aspace.populate_area(
                VirtAddr::from(address as usize).align_down_4k(),
                PAGE_SIZE_4K,
                MappingFlags::WRITE,
            )?;
            let mut bytes = [0u8; 2];
            aspace.read(VirtAddr::from(address as usize), &mut bytes)?;
            debug_assert!(self.counter_journal.len() < self.counter_journal.capacity());
            self.counter_journal
                .push(FixedReplacementCounterJournal { address, bytes });
        }
        Ok(())
    }

    /// Consume only the custody preallocated by `prepare_locked`. Registry
    /// custody is published before the byte patch; a failed patch removes the
    /// record before returning, while every successful INT3 is owned first.
    fn install_prepared_probes(&mut self, aspace: &mut AddrSpace) -> AxResult<()> {
        for prepared in self.incoming_probes.iter().copied() {
            let already_installed = REGISTRY
                .lock()
                .mms
                .get(&self.mm_id)
                .is_some_and(|state| state.installed.contains_key(&prepared.address));
            if already_installed {
                continue;
            }
            let mut decoded = [0u8; 15];
            let readable =
                (PAGE_SIZE_4K - (prepared.address as usize & (PAGE_SIZE_4K - 1))).min(15);
            aspace.read(
                VirtAddr::from(prepared.address as usize),
                &mut decoded[..readable],
            )?;
            let plan = plan_from_bytes(prepared.address, decoded[0], decoded, readable)?;

            // Publish allocation-free custody *before* making INT3
            // executable. The mm lock prevents a competing VMA transition,
            // and trap handling can therefore never observe 0xcc without an
            // InstalledProbe record which names its original instruction.
            {
                let mut registry = REGISTRY.lock();
                let state = registry
                    .mms
                    .get_mut(&self.mm_id)
                    .expect("prepared fixed uprobe mm state disappeared");
                state.installed.insert_remapped(
                    prepared.address,
                    InstalledProbe {
                        address: prepared.address,
                        key: prepared.key,
                        original_byte: decoded[0],
                        retprobe: prepared.retprobe,
                        plan,
                    },
                );
            }
            let original = match aspace
                .uprobe_cow_patch_byte(VirtAddr::from(prepared.address as usize), 0xcc)
            {
                Ok(original) => original,
                Err(error) => {
                    REGISTRY
                        .lock()
                        .mms
                        .get_mut(&self.mm_id)
                        .expect("prepared fixed uprobe mm state disappeared")
                        .installed
                        .remove(&prepared.address);
                    return Err(error);
                }
            };
            if original == 0xcc {
                REGISTRY
                    .lock()
                    .mms
                    .get_mut(&self.mm_id)
                    .expect("prepared fixed uprobe mm state disappeared")
                    .installed
                    .remove(&prepared.address);
                return Err(AxError::InvalidInput);
            }
            if original != decoded[0] {
                // A provider may have supplied a newer resident COW leaf
                // between the pre-read and patch. Keep ownership published
                // while restoring it, re-decode its actual instruction, then
                // republish INT3 with a matching immutable plan.
                // Until a fresh decode has succeeded, a trap must fail safe
                // rather than execute a plan for the stale first byte.
                {
                    let mut registry = REGISTRY.lock();
                    let record = registry
                        .mms
                        .get_mut(&self.mm_id)
                        .expect("prepared fixed uprobe mm state disappeared")
                        .installed
                        .get_mut(&prepared.address)
                        .expect("published uprobe custody disappeared");
                    record.original_byte = original;
                    record.plan = InstructionPlan::RepairPending;
                }
                if let Err(error) = aspace
                    .uprobe_cow_patch_byte(VirtAddr::from(prepared.address as usize), original)
                {
                    // INT3 is still visible and remains owned by the
                    // conservative record above.
                    return Err(error);
                }
                let mut actual = [0u8; 15];
                if let Err(error) = aspace.read(
                    VirtAddr::from(prepared.address as usize),
                    &mut actual[..readable],
                ) {
                    REGISTRY
                        .lock()
                        .mms
                        .get_mut(&self.mm_id)
                        .expect("prepared fixed uprobe mm state disappeared")
                        .installed
                        .remove(&prepared.address);
                    return Err(error);
                }
                let actual_plan =
                    match plan_from_bytes(prepared.address, actual[0], actual, readable) {
                        Ok(plan) => plan,
                        Err(error) => {
                            REGISTRY
                                .lock()
                                .mms
                                .get_mut(&self.mm_id)
                                .expect("prepared fixed uprobe mm state disappeared")
                                .installed
                                .remove(&prepared.address);
                            return Err(error);
                        }
                    };
                let restored = match aspace
                    .uprobe_cow_patch_byte(VirtAddr::from(prepared.address as usize), 0xcc)
                {
                    Ok(byte) => byte,
                    Err(error) => {
                        REGISTRY
                            .lock()
                            .mms
                            .get_mut(&self.mm_id)
                            .expect("prepared fixed uprobe mm state disappeared")
                            .installed
                            .remove(&prepared.address);
                        return Err(error);
                    }
                };
                if restored == 0xcc {
                    REGISTRY
                        .lock()
                        .mms
                        .get_mut(&self.mm_id)
                        .expect("prepared fixed uprobe mm state disappeared")
                        .installed
                        .remove(&prepared.address);
                    return Err(AxError::InvalidInput);
                }
                let mut registry = REGISTRY.lock();
                let record = registry
                    .mms
                    .get_mut(&self.mm_id)
                    .expect("prepared fixed uprobe mm state disappeared")
                    .installed
                    .get_mut(&prepared.address)
                    .expect("published uprobe custody disappeared");
                record.original_byte = restored;
                if restored != actual[0] {
                    // Another byte transition occurred during the repair.
                    // The new CC still has a precise owner, but only the
                    // fail-safe SIGILL plan is valid until deferred retry.
                    record.plan = InstructionPlan::RepairPending;
                    return Err(AxError::BadState);
                }
                record.plan = actual_plan;
                debug_assert!(self.patched.len() < self.patched.capacity());
                self.patched.push((prepared.address, restored));
                continue;
            }
            debug_assert!(self.patched.len() < self.patched.capacity());
            self.patched.push((prepared.address, original));
        }
        Ok(())
    }

    /// Allocation-free counterpart to `sync_reference_counter_locked` for a
    /// prepared fixed replacement.  Every mutable registry node was inserted
    /// as an inert zero-count record in `prepare_locked`; this routine never
    /// removes or inserts nodes while the incoming topology is visible.
    fn sync_prepared_counter(
        &mut self,
        aspace: &mut AddrSpace,
        key: ProbeKey,
        offset: u64,
    ) -> AxResult<()> {
        if offset == 0 {
            return Ok(());
        }
        let address = match mapped_reference_counter(aspace, key.file, offset) {
            ReferenceCounterMapping::Writable(address) => address,
            ReferenceCounterMapping::TemporarilyReadOnly => {
                let mut registry = REGISTRY.lock();
                let state = registry
                    .mms
                    .get_mut(&self.mm_id)
                    .expect("prepared fixed uprobe mm state disappeared");
                // This overwrites the preallocated Reserved marker without a
                // BTree insertion.  mprotect reconciliation will later see
                // the real blocked state exactly as generic sync does.
                let block = state
                    .blocked_reference_counters
                    .get_mut(&(key, offset))
                    .expect("prepared fixed blocked-counter node disappeared");
                *block = CounterBlockState::Blocked;
                return Ok(());
            }
            ReferenceCounterMapping::Absent => {
                let mut registry = REGISTRY.lock();
                let state = registry
                    .mms
                    .get_mut(&self.mm_id)
                    .expect("prepared fixed uprobe mm state disappeared");
                let applied = state
                    .reference_counters
                    .get_mut(&(key, offset))
                    .expect("prepared fixed counter node disappeared");
                applied.address = 0;
                applied.count = 0;
                state.blocked_reference_counters.remove(&(key, offset));
                return Ok(());
            }
        };
        let (desired, current) = {
            let registry = REGISTRY.lock();
            let desired = desired_reference_counter_for_mm(&registry, self.mm_id, key, offset);
            let current = registry
                .mms
                .get(&self.mm_id)
                .and_then(|state| state.reference_counters.get(&(key, offset)))
                .filter(|applied| applied.address == address)
                .map_or(0, |applied| applied.count);
            (desired, current)
        };
        if desired != current {
            let page = VirtAddr::from(address as usize).align_down_4k();
            aspace.populate_area(page, PAGE_SIZE_4K, MappingFlags::WRITE)?;
            let mut bytes = [0u8; 2];
            aspace.read(VirtAddr::from(address as usize), &mut bytes)?;
            let value = u16::from_ne_bytes(bytes);
            let delta =
                u16::try_from(desired.abs_diff(current)).map_err(|_| AxError::InvalidInput)?;
            let value = if desired > current {
                value.checked_add(delta)
            } else {
                value.checked_sub(delta)
            }
            .ok_or(AxError::InvalidInput)?;
            aspace.write(VirtAddr::from(address as usize), &value.to_ne_bytes())?;
        }
        let mut registry = REGISTRY.lock();
        let state = registry
            .mms
            .get_mut(&self.mm_id)
            .expect("prepared fixed uprobe mm state disappeared");
        let applied = state
            .reference_counters
            .get_mut(&(key, offset))
            .expect("prepared fixed counter node disappeared");
        applied.address = address;
        applied.count = desired;
        state.blocked_reference_counters.remove(&(key, offset));
        Ok(())
    }
}

impl crate::mm::FixedReplacementParticipant for PreparedFixedUprobeTransition {
    fn before_install(&mut self, aspace: &mut AddrSpace) -> AxResult {
        debug_assert_eq!(aspace.address_space_id().get(), self.mm_id);

        // The generic MM primitive intentionally does not touch XOL: only
        // this participant has the snapshot required to undo the mutation.
        if self.overlaps_old_xol() {
            invalidate_xol_range_locked(aspace, self.start, self.length);
        }
        if !self.incoming_probes.is_empty() {
            let previous = REGISTRY
                .lock()
                .mms
                .get(&self.mm_id)
                .map_or(0, |state| state.xol_base);
            let base = ensure_xol_mapping_locked_avoiding(
                &self.aspace,
                aspace,
                Some((self.start, self.length)),
            );
            let Ok(base) = base else {
                // Linux's uprobe_mmap is explicitly best effort. The fixed
                // mapping remains successful; a later deferred reconciliation
                // retries XOL and instruction installation.
                self.defer_mmap_reconcile();
                return Ok(());
            };
            if previous != 0 && previous != base {
                self.created_xol_base = Some(base);
            } else if previous == 0 {
                self.created_xol_base = Some(base);
            }
            self.instrumentation_ready = true;
        }
        Ok(())
    }

    fn commit(&mut self, aspace: &mut AddrSpace) -> AxResult {
        debug_assert_eq!(aspace.address_space_id().get(), self.mm_id);
        // Retire only old records in the replaced range.  The old PTEs are
        // still held by the memory-set guard, but they are no longer
        // reachable from the incoming VMA and must not suppress a fresh INT3
        // installation at the same address.
        if let Some(state) = REGISTRY.lock().mms.get_mut(&self.mm_id) {
            let first = self.start.as_usize() as u64;
            let end = first.saturating_add(self.length as u64);
            state
                .installed
                .retain(|address, _| *address < first || *address >= end);
        }
        if !self.instrumentation_ready {
            // The replacement is already a success.  Do not expose a probe
            // whose trap path has no valid uprobe/uretprobe trampoline; the
            // deferred reconcile establishes XOL before publishing INT3.
            self.defer_mmap_reconcile();
            return Ok(());
        }
        // Retirement above is unconditional for a successful VMA replace.
        // Every following instrumentation failure is best-effort and leaves
        // this exact ownership state for the deferred reconciliation worker.
        if self.journal_counter_bytes(aspace).is_err() {
            self.defer_mmap_reconcile();
            return Ok(());
        }
        if self.install_prepared_probes(aspace).is_err() {
            self.defer_mmap_reconcile();
            return Ok(());
        }
        for index in 0..self.counter_identities.len() {
            let (key, offset) = self.counter_identities[index];
            if self.sync_prepared_counter(aspace, key, offset).is_err() {
                self.defer_mmap_reconcile();
                return Ok(());
            }
        }
        // No fallible work follows this point in the participant.  Discard
        // inert reservation nodes so deferred-retirement accounting continues
        // to mean "a real applied contribution exists", not merely "a fixed
        // replacement once reserved capacity here".
        if let Some(state) = REGISTRY.lock().mms.get_mut(&self.mm_id) {
            state
                .reference_counters
                .retain(|_, applied| applied.count != 0);
            state
                .blocked_reference_counters
                .retain(|_, state| *state != CounterBlockState::Reserved);
        }
        Ok(())
    }

    fn rollback(&mut self, aspace: &mut AddrSpace) -> AxResult {
        let mut rollback_error = None;
        // A probe is journaled immediately after its INT3 write and before
        // registry publication.  Therefore this loop also covers the former
        // unpublished-patch hole: a later decode/counter failure cannot strand
        // an INT3 merely because no InstalledProbe was ever inserted.
        for (address, original) in self.patched.iter().copied().rev() {
            if let Err(error) =
                aspace.uprobe_cow_patch_byte(VirtAddr::from(address as usize), original)
            {
                rollback_error.get_or_insert(error);
            }
        }
        for journal in self.counter_journal.iter().copied().rev() {
            if let Err(error) =
                aspace.write(VirtAddr::from(journal.address as usize), &journal.bytes)
            {
                rollback_error.get_or_insert(error);
            }
        }
        if let Some(base) = self.created_xol_base.take() {
            // This page was created by the provisional incoming installation,
            // never existed in the snapshot, and is outside the replacement
            // range (it was chosen from the live old VMA topology).
            if let Err(error) = aspace.unmap(VirtAddr::from(base as usize), PAGE_SIZE_4K) {
                rollback_error.get_or_insert(error);
            }
        }
        let mut registry = REGISTRY.lock();
        match self.old_state.take() {
            Some(old) => {
                registry.mms.insert(self.mm_id, old);
            }
            None => {
                registry.mms.remove(&self.mm_id);
            }
        }
        rollback_error.map_or(Ok(()), Err)
    }
}

/// Retire breakpoint ownership for a range after a destructive VMA operation
/// has removed its old mappings.  No byte restoration is attempted: the COW
/// pages which contained those bytes are no longer reachable in this mm.
/// Callers must hold both the topology gate and the mm lock, and must invoke
/// this only after the corresponding unmap/replace commit has succeeded.
pub(crate) fn retire_unmapped_probe_range_locked(
    aspace: &mut AddrSpace,
    start: VirtAddr,
    length: usize,
) -> AxResult<()> {
    let first = start.as_usize() as u64;
    let end = first.saturating_add(length as u64);
    let mm_id = aspace.address_space_id().get();
    let mut registry = REGISTRY.lock();
    let Some(state) = registry.mms.get_mut(&mm_id) else {
        return Ok(());
    };
    state
        .installed
        .retain(|address, _| *address < first || *address >= end);
    let counters = state.reference_counters.keys().copied().collect::<Vec<_>>();
    drop(registry);
    let mut retry = false;
    for (key, offset) in counters {
        if sync_reference_counter_locked(aspace, key, offset, None).is_err() {
            retry = true;
        }
    }
    if retry {
        if let Some(state) = REGISTRY.lock().mms.get_mut(&mm_id) {
            state.pending_mmap_reconcile = true;
        }
        crate::deferred_work::wake_uprobe_restore_worker();
    }
    Ok(())
}

fn address_maps_file_offset(
    mm: &AddrSpace,
    address: u64,
    file: UprobeFileKey,
    file_offset: u64,
) -> bool {
    mm.find_area(VirtAddr::from(address as usize))
        .and_then(|area| {
            let mapping = area.backend().file_mapping()?;
            let identity = mapping.identity();
            Some(
                mapping.sharing() == crate::mm::FileMappingSharing::Private
                    && identity.mount_id() == file.mount_id
                    && identity.device() == file.device
                    && identity.inode() == file.inode
                    && mapping.file_offset_at(VirtAddr::from(address as usize))
                        == Some(file_offset),
            )
        })
        .unwrap_or(false)
}

pub(crate) struct PreparedRemapTopologyTransfer {
    probes: Vec<InstalledProbe>,
    counters: Vec<((ProbeKey, u64), AppliedReferenceCounter)>,
}

/// Allocate every snapshot needed for remap custody before the VMA commit.
/// The matching commit routine is allocation-free apart from registry node
/// replacement, whose destination keys are already represented by this plan.
pub(crate) fn prepare_remap_topology_transfer_locked(
    mm: &AddrSpace,
    source: VirtAddr,
    source_size: usize,
    destination: VirtAddr,
    destination_size: usize,
) -> AxResult<PreparedRemapTopologyTransfer> {
    let mm_id = mm.address_space_id().get();
    let source_first = source.as_usize() as u64;
    let source_end = source_first.saturating_add(source_size as u64);
    let destination_first = destination.as_usize() as u64;
    let destination_end = destination_first.saturating_add(destination_size as u64);
    let mut registry = REGISTRY.lock();
    let Some(state) = registry.mms.get_mut(&mm_id) else {
        return Ok(PreparedRemapTopologyTransfer {
            probes: Vec::new(),
            counters: Vec::new(),
        });
    };
    let probe_count = state
        .installed
        .values()
        .filter(|probe| probe.address >= source_first && probe.address < source_end)
        .count();
    let counter_count = state
        .reference_counters
        .values()
        .filter(|applied| {
            (applied.address >= source_first && applied.address < source_end)
                || (applied.address >= destination_first && applied.address < destination_end)
        })
        .count();
    let mut probes = Vec::new();
    let mut counters = Vec::new();
    // The destination entries themselves also need fallible capacity before
    // commit; BTreeMap cannot reserve nodes, so commit publishes them through
    // the pre-reserved linear custody tier.
    state.installed.try_reserve_remapped(probe_count)?;
    probes
        .try_reserve_exact(probe_count)
        .map_err(|_| AxError::NoMemory)?;
    counters
        .try_reserve_exact(counter_count)
        .map_err(|_| AxError::NoMemory)?;
    probes.extend(
        state
            .installed
            .values()
            .filter(|probe| probe.address >= source_first && probe.address < source_end)
            .copied(),
    );
    counters.extend(
        state
            .reference_counters
            .iter()
            .filter_map(|(identity, applied)| {
                ((applied.address >= source_first && applied.address < source_end)
                    || (applied.address >= destination_first && applied.address < destination_end))
                    .then_some((*identity, *applied))
            }),
    );
    Ok(PreparedRemapTopologyTransfer { probes, counters })
}

/// Transfer already-published INT3 and USDT semaphore custody across the
/// concrete VMA move/duplicate transaction. This runs before the general
/// reconciliation pass and consumes only pre-commit snapshots.
pub(crate) fn commit_remap_topology_transfer_locked(
    mm: &mut AddrSpace,
    prepared: PreparedRemapTopologyTransfer,
    source: VirtAddr,
    source_size: usize,
    destination: VirtAddr,
    destination_size: usize,
    duplicate: bool,
    commit_succeeded: bool,
) {
    let mm_id = mm.address_space_id().get();
    let source_first = source.as_usize() as u64;
    let source_end = source_first.saturating_add(source_size as u64);
    let destination_first = destination.as_usize() as u64;
    let destination_end = destination_first.saturating_add(destination_size as u64);
    let transferable = source_size.min(destination_size) as u64;
    let mut registry = REGISTRY.lock();
    let Some(state) = registry.mms.get_mut(&mm_id) else {
        return;
    };
    // A destructive failure is allowed to leave the old destination intact.
    // Retire only records whose exact file-offset/INT3 ownership has actually
    // disappeared; never infer destruction merely from the requested range.
    state.installed.retain(|address, installed| {
        if *address < destination_first || *address >= destination_end {
            return true;
        }
        let mut byte = [0u8; 1];
        address_maps_file_offset(mm, *address, installed.key.file, installed.key.offset)
            && mm
                .read(VirtAddr::from(*address as usize), &mut byte)
                .is_ok()
            && byte[0] == 0xcc
    });
    for mut probe in prepared.probes {
        let relative = probe.address - source_first;
        if relative >= transferable {
            continue;
        }
        let target = destination_first + relative;
        let mut byte = [0u8; 1];
        let target_owns_int3 =
            address_maps_file_offset(mm, target, probe.key.file, probe.key.offset)
                && mm.read(VirtAddr::from(target as usize), &mut byte).is_ok()
                && byte[0] == 0xcc;
        let destination_has_owner = state.installed.contains_key(&target);
        if target_owns_int3 && (commit_succeeded || !destination_has_owner) {
            probe.address = target;
            state.installed.insert_remapped(target, probe);
        }
        if !duplicate
            && (!address_maps_file_offset(
                mm,
                source_first + relative,
                probe.key.file,
                probe.key.offset,
            ) || mm
                .read(
                    VirtAddr::from((source_first + relative) as usize),
                    &mut byte,
                )
                .is_err()
                || byte[0] != 0xcc)
        {
            state.installed.remove(&(source_first + relative));
        }
    }

    for ((key, offset), applied) in prepared.counters {
        if applied.address >= destination_first
            && applied.address < destination_end
            && !(applied.address >= source_first && applied.address < source_end)
            && !address_maps_file_offset(mm, applied.address, key.file, offset)
        {
            state.reference_counters.remove(&(key, offset));
            state.blocked_reference_counters.remove(&(key, offset));
            continue;
        }
        if commit_succeeded
            && duplicate
            && applied.address >= source_first
            && applied.address < source_end
        {
            let relative = applied.address - source_first;
            if relative < transferable {
                let target = destination_first + relative;
                if address_maps_file_offset(mm, target, key.file, offset)
                    && matches!(
                        mapped_reference_counter(mm, key.file, offset),
                        ReferenceCounterMapping::Writable(selected) if selected == target
                    )
                    && let Some(current) = state.reference_counters.get_mut(&(key, offset))
                {
                    // DONTUNMAP copied the already incremented COW content.
                    // Track whichever alias the normal lookup will select so
                    // reconciliation does not add the contribution twice.
                    current.address = target;
                }
            }
            continue;
        }
        if applied.address < source_first
            || applied.address >= source_end
            || address_maps_file_offset(mm, applied.address, key.file, offset)
        {
            continue;
        }
        let relative = applied.address - source_first;
        if relative >= transferable {
            continue;
        }
        let target = destination_first + relative;
        if address_maps_file_offset(mm, target, key.file, offset)
            && let Some(current) = state.reference_counters.get_mut(&(key, offset))
        {
            current.address = target;
        }
    }
}

/// Apply one registered file-offset probe to all matching executable private
/// VMAs of an mm.  A shared mapping is intentionally skipped: Linux uprobes
/// must not alter the file page cache or another process's mapping.
fn matching_probe_addresses(mm: &AddrSpace, file: UprobeFileKey, offset: u64) -> Vec<u64> {
    let mut addresses = Vec::new();
    for area in mm.areas() {
        if !area.flags().contains(MappingFlags::EXECUTE) {
            continue;
        }
        let Some(mapping) = area.backend().file_mapping() else {
            continue;
        };
        if mapping.sharing() != crate::mm::FileMappingSharing::Private {
            continue;
        }
        let identity = mapping.identity();
        if identity.mount_id() != file.mount_id
            || identity.device() != file.device
            || identity.inode() != file.inode
        {
            continue;
        }
        let Some(first) = mapping.file_offset_at(area.start()) else {
            continue;
        };
        let length = (area.end().as_usize() - area.start().as_usize()) as u64;
        if offset >= first && offset < first.saturating_add(length) {
            addresses.push(area.start().as_usize() as u64 + (offset - first));
        }
    }
    addresses
}

pub(crate) fn install_for_mm(
    aspace: &Arc<Mutex<AddrSpace>>,
    file: UprobeFileKey,
    offset: u64,
    retprobe: bool,
    reference_counter_offset: u64,
) -> AxResult<()> {
    if matching_probe_addresses(&aspace.lock(), file, offset).is_empty() {
        return Err(AxError::NotFound);
    }
    ensure_xol_mapping(aspace)?;
    // Hold the VMA transaction across semaphore activation and every COW
    // instruction replacement.  No concurrent mprotect can make rollback
    // impossible after the userspace counter has been changed.
    let mut mm = aspace.lock();
    if install_for_mm_locked(
        aspace,
        &mut mm,
        file,
        offset,
        retprobe,
        reference_counter_offset,
    )? {
        Ok(())
    } else {
        Err(AxError::NotFound)
    }
}

fn install_for_mm_locked(
    aspace: &Arc<Mutex<AddrSpace>>,
    mm: &mut AddrSpace,
    file: UprobeFileKey,
    offset: u64,
    retprobe: bool,
    reference_counter_offset: u64,
) -> AxResult<bool> {
    let key = ProbeKey { file, offset };
    let addresses = matching_probe_addresses(mm, file, offset);
    if addresses.is_empty() {
        return Ok(false);
    }
    let mm_id = mm.address_space_id().get();
    let (retprobe, had_installed_key, desired_counter) = {
        let mut registry = REGISTRY.lock();
        let retprobe = retprobe
            || registry
                .consumers
                .get(&key)
                .is_some_and(|refs| refs.returns != 0);
        let state = registry.mms.entry(mm_id).or_default();
        state.aspace = Arc::downgrade(aspace);
        state.installed.try_reserve_remapped(addresses.len())?;
        if reference_counter_offset != 0 {
            state
                .reference_counters
                .entry((key, reference_counter_offset))
                .or_insert(AppliedReferenceCounter {
                    address: 0,
                    count: 0,
                });
        }
        let had_installed = state.installed.values().any(|probe| probe.key == key);
        let desired_counter = desired_reference_counter(&registry, key, reference_counter_offset);
        (retprobe, had_installed, desired_counter)
    };
    let mut patched = Vec::new();
    let mut published = Vec::new();
    patched
        .try_reserve_exact(addresses.len())
        .map_err(|_| AxError::NoMemory)?;
    published
        .try_reserve_exact(addresses.len())
        .map_err(|_| AxError::NoMemory)?;
    // Every allocation-bearing journal and the counter BTree node were
    // admitted above. Only now may the generic synchronizer write a visible
    // USDT semaphore contribution.
    sync_reference_counter_locked(mm, key, reference_counter_offset, Some(desired_counter))?;
    for address in addresses {
        let already = REGISTRY
            .lock()
            .mms
            .get(&mm_id)
            .is_some_and(|state| state.installed.contains_key(&address));
        if already {
            continue;
        }
        let mut decoded = [0u8; 15];
        let readable = (PAGE_SIZE_4K - (address as usize & (PAGE_SIZE_4K - 1))).min(15);
        if let Err(error) = mm.read(VirtAddr::from(address as usize), &mut decoded[..readable]) {
            rollback_prepublished_mapping_install(mm, mm_id, &patched, &published);
            if !had_installed_key {
                let _ = sync_reference_counter_locked(mm, key, reference_counter_offset, Some(0));
            }
            return Err(error);
        }
        let original = decoded[0];
        let plan = match plan_from_bytes(address, original, decoded, readable) {
            Ok(plan) => plan,
            Err(error) => {
                rollback_prepublished_mapping_install(mm, mm_id, &patched, &published);
                if !had_installed_key {
                    let _ =
                        sync_reference_counter_locked(mm, key, reference_counter_offset, Some(0));
                }
                return Err(error);
            }
        };
        {
            let mut registry = REGISTRY.lock();
            let state = registry
                .mms
                .get_mut(&mm_id)
                .expect("uprobe mm state disappeared");
            state.installed.insert_remapped(
                address,
                InstalledProbe {
                    address,
                    key,
                    original_byte: original,
                    retprobe,
                    plan,
                },
            );
        }
        debug_assert!(published.len() < published.capacity());
        published.push(address);
        let original = match mm.uprobe_cow_patch_byte(VirtAddr::from(address as usize), 0xcc) {
            Ok(byte) if byte != 0xcc => byte,
            Ok(_) => {
                rollback_prepublished_mapping_install(mm, mm_id, &patched, &published);
                if !had_installed_key {
                    let _ =
                        sync_reference_counter_locked(mm, key, reference_counter_offset, Some(0));
                }
                return Err(AxError::InvalidInput);
            }
            Err(error) => {
                rollback_prepublished_mapping_install(mm, mm_id, &patched, &published);
                if !had_installed_key {
                    let _ =
                        sync_reference_counter_locked(mm, key, reference_counter_offset, Some(0));
                }
                return Err(error);
            }
        };
        if original != decoded[0] {
            if let Some(record) = REGISTRY
                .lock()
                .mms
                .get_mut(&mm_id)
                .and_then(|state| state.installed.get_mut(&address))
            {
                record.original_byte = original;
                record.plan = InstructionPlan::RepairPending;
            }
            debug_assert!(patched.len() < patched.capacity());
            patched.push((address, original, retprobe, plan));
            rollback_prepublished_mapping_install(mm, mm_id, &patched, &published);
            if !had_installed_key {
                let _ = sync_reference_counter_locked(mm, key, reference_counter_offset, Some(0));
            }
            return Err(AxError::BadState);
        }
        debug_assert!(patched.len() < patched.capacity());
        patched.push((address, original, retprobe, plan));
    }
    let mut registry = REGISTRY.lock();
    let state = registry.mms.entry(mm_id).or_default();
    state.aspace = Arc::downgrade(aspace);
    for installed in state
        .installed
        .values_mut()
        .filter(|probe| probe.key == key)
    {
        installed.retprobe |= retprobe;
    }
    let _ = patched;
    Ok(true)
}

/// Reapply every registered file-offset probe to a freshly built or newly
/// extended mm. Missing mappings are normal: registration is independent of
/// ELF load order and later mmap uses this same routine before returning to
/// userspace.
pub(crate) fn install_all_for_mm(aspace: &Arc<Mutex<AddrSpace>>) -> AxResult<()> {
    let _topology = registration_topology_gate();
    install_all_for_mm_gated(aspace)
}

pub(crate) fn install_all_for_mm_gated(aspace: &Arc<Mutex<AddrSpace>>) -> AxResult<()> {
    let mut mm = aspace.lock();
    install_all_for_mm_locked_gated(aspace, &mut mm)
}

fn install_all_for_mm_locked_gated(
    aspace: &Arc<Mutex<AddrSpace>>,
    mm: &mut AddrSpace,
) -> AxResult<()> {
    let mm_id = mm.address_space_id().get();
    // Repair markers are distinct from legitimate ControlledUnsupported
    // plans. Restore their owned byte, drop custody, then let the ordinary
    // installer predecode and publish a fresh plan below.
    let repairs = REGISTRY
        .lock()
        .mms
        .get(&mm_id)
        .map(|state| {
            state
                .installed
                .values()
                .filter(|probe| matches!(probe.plan, InstructionPlan::RepairPending))
                .map(|probe| (probe.address, probe.original_byte))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for (address, original) in repairs {
        mm.uprobe_cow_patch_byte(VirtAddr::from(address as usize), original)?;
        REGISTRY
            .lock()
            .mms
            .get_mut(&mm_id)
            .expect("repair mm state disappeared")
            .installed
            .remove(&address);
    }
    let (probes, counters, applied) = {
        let registry = REGISTRY.lock();
        let probes = registry
            .consumers
            .iter()
            .filter(|(_, refs)| refs.total != 0)
            .map(|(key, refs)| (*key, refs.returns != 0))
            .collect::<Vec<_>>();
        let counters = registry
            .consumers
            .iter()
            .flat_map(|(key, refs)| {
                refs.reference_counters
                    .iter()
                    .filter(|counter| counter.count != 0)
                    .map(|counter| (*key, counter.offset))
            })
            .collect::<Vec<_>>();
        let applied = registry
            .mms
            .get(&mm_id)
            .map(|state| state.reference_counters.keys().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        (probes, counters, applied)
    };
    if probes
        .iter()
        .any(|(key, _)| !matching_probe_addresses(mm, key.file, key.offset).is_empty())
    {
        ensure_xol_mapping_locked(aspace, mm)?;
    }
    for (key, retprobe) in probes {
        install_for_mm_locked(aspace, mm, key.file, key.offset, retprobe, 0)?;
    }
    for (key, offset) in counters {
        let installed = REGISTRY
            .lock()
            .mms
            .get(&mm_id)
            .is_some_and(|state| state.installed.values().any(|probe| probe.key == key));
        if installed {
            sync_reference_counter_locked(mm, key, offset, None)?;
        }
    }
    // Applied contributions must also be reconciled after unregister or an
    // mprotect transition even though the consumer no longer appears in the
    // active counter list.
    for (key, offset) in applied {
        sync_reference_counter_locked(mm, key, offset, None)?;
    }
    Ok(())
}

/// Reconcile registered file offsets with the VMAs left by munmap, mprotect,
/// and mremap. A moved/duplicated private COW page can already contain the
/// uprobe INT3; in that case its authoritative original byte is transferred
/// from the old address instead of treating the retained breakpoint as an
/// invalid user instruction. New executable aliases are installed normally
/// after stale addresses have been forgotten.
pub(crate) fn reconcile_mm(aspace: &Arc<Mutex<AddrSpace>>) -> AxResult<()> {
    let _topology = registration_topology_gate();
    reconcile_mm_gated(aspace)
}

pub(crate) fn reconcile_mm_gated(aspace: &Arc<Mutex<AddrSpace>>) -> AxResult<()> {
    let mut mm = aspace.lock();
    reconcile_mm_locked_gated(aspace, &mut mm)
}

pub(crate) fn reconcile_mm_locked_gated(
    aspace: &Arc<Mutex<AddrSpace>>,
    mm: &mut AddrSpace,
) -> AxResult<()> {
    let mm_id = mm.address_space_id().get();
    let mut registry = REGISTRY.lock();
    let probes = registry
        .consumers
        .iter()
        .filter(|(_, refs)| refs.total != 0)
        .map(|(key, refs)| (*key, refs.returns != 0))
        .collect::<Vec<_>>();
    let templates = registry
        .mms
        .get(&mm_id)
        .map(|state| state.installed.values().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    let mut desired = Vec::new();
    desired
        .try_reserve(probes.len().saturating_mul(mm.areas().count()))
        .map_err(|_| AxError::NoMemory)?;
    for area in mm.areas() {
        if !area.flags().contains(MappingFlags::EXECUTE) {
            continue;
        }
        let Some(mapping) = area.backend().file_mapping() else {
            continue;
        };
        if mapping.sharing() != crate::mm::FileMappingSharing::Private {
            continue;
        }
        let identity = mapping.identity();
        let file = UprobeFileKey {
            mount_id: identity.mount_id(),
            device: identity.device(),
            inode: identity.inode(),
        };
        let Some(first) = mapping.file_offset_at(area.start()) else {
            continue;
        };
        let length = (area.end().as_usize() - area.start().as_usize()) as u64;
        for (key, retprobe) in probes.iter().copied().filter(|(key, _)| key.file == file) {
            if key.offset < first || key.offset >= first.saturating_add(length) {
                continue;
            }
            let address = area.start().as_usize() as u64 + (key.offset - first);
            let mut byte = [0u8; 1];
            if mm.read(VirtAddr::from(address as usize), &mut byte).is_ok() {
                desired.push((address, key, retprobe, byte[0]));
            }
        }
    }

    let xol_valid = registry
        .mms
        .get(&mm_id)
        .is_none_or(|state| is_exact_xol_vma(mm, state.xol_base, state.xol_token));
    let state = registry.mms.entry(mm_id).or_default();
    state.aspace = Arc::downgrade(aspace);
    // A failed final-consumer restore remains owned by `installed`.  Do not
    // simply forget such a record when there is no longer a desired consumer:
    // that would strand an INT3 in a live executable mapping.  If the address
    // has been unmapped or replaced with a different file/offset, the old COW
    // overlay disappeared with that VMA and the record can be retired.  For
    // the same mapping, restore the authoritative byte under this mm lock and
    // retain ownership on every transient failure so a later lifecycle
    // reconciliation retries it.
    let stale = state
        .installed
        .iter()
        .filter(|(address, installed)| {
            !desired
                .iter()
                .any(|(wanted, key, ..)| wanted == *address && *key == installed.key)
        })
        .map(|(address, installed)| (*address, *installed))
        .collect::<Vec<_>>();
    for (address, installed) in stale {
        let mapped_to_same_probe = mm
            .find_area(VirtAddr::from(address as usize))
            .and_then(|area| {
                let mapping = area.backend().file_mapping()?;
                let identity = mapping.identity();
                let file = UprobeFileKey {
                    mount_id: identity.mount_id(),
                    device: identity.device(),
                    inode: identity.inode(),
                };
                Some(
                    mapping.sharing() == crate::mm::FileMappingSharing::Private
                        && file == installed.key.file
                        && mapping.file_offset_at(VirtAddr::from(address as usize))
                            == Some(installed.key.offset),
                )
            })
            .unwrap_or(false);
        if !mapped_to_same_probe
            || mm
                .uprobe_cow_patch_byte(VirtAddr::from(address as usize), installed.original_byte)
                .is_ok()
        {
            state.installed.remove(&address);
        }
    }
    for (address, key, retprobe, byte) in &desired {
        if let Some(installed) = state.installed.get_mut(address) {
            installed.retprobe = *retprobe;
            continue;
        }
        if *byte != 0xcc {
            continue;
        }
        if let Some(template) = templates.iter().find(|template| template.key == *key) {
            state.installed.insert(
                *address,
                InstalledProbe {
                    address: *address,
                    key: *key,
                    original_byte: template.original_byte,
                    retprobe: *retprobe,
                    plan: template.plan,
                },
            );
        }
    }
    if !xol_valid {
        state.xol_base = 0;
        state.trampoline_base = 0;
        state.xol_generation = 0;
        state.xol_token = 0;
        state.xol_slots.clear();
    }
    drop(registry);
    install_all_for_mm_locked_gated(aspace, mm)
}

fn rollback_mapping_install(
    aspace: &mut AddrSpace,
    patched: &[(u64, u8, bool, InstructionPlan)],
    activated: &[(ProbeKey, u64)],
) {
    for (address, original, ..) in patched {
        let _ = aspace.uprobe_cow_patch_byte(VirtAddr::from(*address as usize), *original);
    }
    for (key, offset) in activated {
        let _ = sync_reference_counter_locked(aspace, *key, *offset, Some(0));
    }
}

/// Roll back one installer which published allocation-free custody before
/// each INT3 write.  `published` includes records whose patch never started;
/// removing all of them is therefore as important as restoring successful
/// breakpoint bytes.
fn rollback_prepublished_mapping_install(
    aspace: &mut AddrSpace,
    mm_id: u64,
    patched: &[(u64, u8, bool, InstructionPlan)],
    published: &[u64],
) -> bool {
    // `patched` was pre-reserved by every caller before publication; reuse a
    // stack-only membership scan below rather than allocating while failing.
    let mut all_restored = true;
    for (address, original, ..) in patched {
        if aspace
            .uprobe_cow_patch_byte(VirtAddr::from(*address as usize), *original)
            .is_ok()
        {
            // Successful owners may be removed below.
        } else {
            all_restored = false;
            if let Some(record) = REGISTRY
                .lock()
                .mms
                .get_mut(&mm_id)
                .and_then(|state| state.installed.get_mut(address))
            {
                record.plan = InstructionPlan::RepairPending;
            }
        }
    }
    if let Some(state) = REGISTRY.lock().mms.get_mut(&mm_id) {
        for address in published.iter().copied() {
            let restore_failed = patched.iter().any(|(patched_address, original, ..)| {
                *patched_address == address
                    // A failed restore is represented by the repair marker
                    // set above; do not infer ownership from a second user
                    // memory read.
                    && state.installed.get(&address).is_some_and(|probe| {
                        matches!(probe.plan, InstructionPlan::RepairPending)
                            && probe.original_byte == *original
                    })
            });
            if !restore_failed {
                state.installed.remove(&address);
            }
        }
        if !all_restored {
            state.pending_mmap_reconcile = true;
        }
    }
    if !all_restored {
        crate::deferred_work::wake_uprobe_restore_worker();
    }
    all_restored
}

/// Commit probes for the VMA that has just been inserted, while the caller
/// still holds the mmap transaction lock.  All bytes are COW-patched before a
/// registry record is published; on failure every byte patched by this call is
/// restored and mmap rolls the VMA back before it becomes a successful syscall.
pub(crate) fn install_mapping_locked(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    aspace: &mut AddrSpace,
    start: VirtAddr,
    length: usize,
) -> AxResult<()> {
    install_mapping_locked_inner(aspace_handle, aspace, start, length, false)
}

/// Linux `uprobe_mmap()` is post-publication best effort.  An instruction
/// decode, XOL allocation, COW or semaphore activation failure must never
/// turn an otherwise valid ordinary mmap into an error or tear its VMA down.
/// The transactional projected-exec installer intentionally does not use
/// this wrapper.
pub(crate) fn install_mapping_best_effort_locked(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    aspace: &mut AddrSpace,
    start: VirtAddr,
    length: usize,
) {
    if install_mapping_locked(aspace_handle, aspace, start, length).is_err() {
        defer_mmap_reconcile_locked(aspace_handle, aspace.address_space_id().get());
    }
}

/// Preinstalls probes for a private file VMA which an already-authorized
/// mprotect transaction is about to make executable.  Publication happens
/// while execution is still impossible, so a later protection commit cannot
/// expose an uninstrumented instruction stream even if generic reconciliation
/// subsequently needs deferred cleanup.
pub(crate) fn install_projected_exec_mapping_locked(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    aspace: &mut AddrSpace,
    start: VirtAddr,
    length: usize,
) -> AxResult<()> {
    install_mapping_locked_inner(aspace_handle, aspace, start, length, true)
}

fn install_mapping_locked_inner(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    aspace: &mut AddrSpace,
    start: VirtAddr,
    length: usize,
    projected_execute: bool,
) -> AxResult<()> {
    let mm_id = aspace.address_space_id().get();
    {
        let mut registry = REGISTRY.lock();
        registry.mms.entry(mm_id).or_default().aspace = Arc::downgrade(aspace_handle);
    }
    let Some(area) = aspace.areas().find(|area| {
        area.start() <= start && area.end().as_usize() >= start.as_usize().saturating_add(length)
    }) else {
        return Err(AxError::BadState);
    };
    if !area.flags().contains(MappingFlags::EXECUTE) && !projected_execute {
        // The writable semaphore VMA is commonly mapped after the executable
        // image.  Only probes already installed in this mm may acquire a
        // contribution when that second mapping appears.
        let counters = {
            let registry = REGISTRY.lock();
            let installed = registry.mms.get(&mm_id);
            registry
                .consumers
                .iter()
                .filter(|(key, refs)| {
                    refs.total != 0
                        && installed.is_some_and(|state| {
                            state.installed.values().any(|probe| probe.key == **key)
                        })
                })
                .flat_map(|(key, refs)| {
                    refs.reference_counters
                        .iter()
                        .filter(|counter| counter.count != 0)
                        .map(|counter| (*key, counter.offset))
                })
                .collect::<Vec<_>>()
        };
        // Admit every registry node before generic sync can populate/write a
        // user-visible semaphore.  The zero-count records are inert until
        // the synchronizer publishes an actual contribution.
        {
            let mut registry = REGISTRY.lock();
            let state = registry.mms.entry(mm_id).or_default();
            state.aspace = Arc::downgrade(aspace_handle);
            for identity in counters.iter().copied() {
                state
                    .reference_counters
                    .entry(identity)
                    .or_insert(AppliedReferenceCounter {
                        address: 0,
                        count: 0,
                    });
            }
        }
        for (key, offset) in counters {
            sync_reference_counter_locked(aspace, key, offset, None)?;
        }
        return Ok(());
    }
    let Some(mapping) = area.backend().file_mapping() else {
        return Ok(());
    };
    if mapping.sharing() != crate::mm::FileMappingSharing::Private {
        return Ok(());
    }
    let identity = mapping.identity();
    let file = UprobeFileKey {
        mount_id: identity.mount_id(),
        device: identity.device(),
        inode: identity.inode(),
    };
    let Some(first) = mapping.file_offset_at(start) else {
        return Ok(());
    };
    let end = first.saturating_add(length as u64);
    let keys = REGISTRY
        .lock()
        .consumers
        .iter()
        .filter(|(key, refs)| {
            refs.total != 0 && key.file == file && key.offset >= first && key.offset < end
        })
        .map(|(key, refs)| {
            (
                *key,
                refs.returns != 0,
                refs.reference_counters
                    .iter()
                    .find(|counter| counter.count != 0)
                    .map(|counter| (counter.offset, counter.count)),
            )
        })
        .collect::<Vec<_>>();
    if !keys.is_empty() {
        ensure_xol_mapping_locked(aspace_handle, aspace)?;
    }
    {
        let mut registry = REGISTRY.lock();
        let state = registry.mms.entry(mm_id).or_default();
        state.aspace = Arc::downgrade(aspace_handle);
        state.installed.try_reserve_remapped(keys.len())?;
        for (key, _, counter) in &keys {
            if let Some((offset, _)) = counter
                && *offset != 0
            {
                state.reference_counters.entry((*key, *offset)).or_insert(
                    AppliedReferenceCounter {
                        address: 0,
                        count: 0,
                    },
                );
            }
        }
    }
    let mut patched = Vec::new();
    let mut activated = Vec::new();
    let mut published = Vec::new();
    patched
        .try_reserve_exact(keys.len())
        .map_err(|_| AxError::NoMemory)?;
    activated
        .try_reserve_exact(keys.len())
        .map_err(|_| AxError::NoMemory)?;
    published
        .try_reserve_exact(keys.len())
        .map_err(|_| AxError::NoMemory)?;
    for (key, retprobe, reference_counter) in keys {
        if let Some((reference_counter_offset, desired_counter)) = reference_counter {
            let was_applied = REGISTRY.lock().mms.get(&mm_id).is_some_and(|state| {
                state
                    .reference_counters
                    .get(&(key, reference_counter_offset))
                    .is_some_and(|applied| applied.count != 0)
            });
            if let Err(error) = sync_reference_counter_locked(
                aspace,
                key,
                reference_counter_offset,
                Some(desired_counter),
            ) {
                rollback_prepublished_mapping_install(aspace, mm_id, &patched, &published);
                for (active_key, active_offset) in &activated {
                    let _ =
                        sync_reference_counter_locked(aspace, *active_key, *active_offset, Some(0));
                }
                return Err(error);
            }
            let is_applied = REGISTRY.lock().mms.get(&mm_id).is_some_and(|state| {
                state
                    .reference_counters
                    .get(&(key, reference_counter_offset))
                    .is_some_and(|applied| applied.count != 0)
            });
            if !was_applied && is_applied {
                activated.push((key, reference_counter_offset));
            }
        }
        let address = start.as_usize() as u64 + (key.offset - first);
        if REGISTRY
            .lock()
            .mms
            .get(&mm_id)
            .is_some_and(|mm| mm.installed.contains_key(&address))
        {
            continue;
        }
        let mut decoded = [0u8; 15];
        let readable = (PAGE_SIZE_4K - (address as usize & (PAGE_SIZE_4K - 1))).min(15);
        if let Err(error) = aspace.read(VirtAddr::from(address as usize), &mut decoded[..readable])
        {
            rollback_prepublished_mapping_install(aspace, mm_id, &patched, &published);
            for (active_key, active_offset) in &activated {
                let _ = sync_reference_counter_locked(aspace, *active_key, *active_offset, Some(0));
            }
            return Err(error);
        }
        let original = decoded[0];
        let plan = match plan_from_bytes(address, original, decoded, readable) {
            Ok(plan) => plan,
            Err(error) => {
                rollback_prepublished_mapping_install(aspace, mm_id, &patched, &published);
                for (active_key, active_offset) in &activated {
                    let _ =
                        sync_reference_counter_locked(aspace, *active_key, *active_offset, Some(0));
                }
                return Err(error);
            }
        };
        {
            let mut registry = REGISTRY.lock();
            let state = registry
                .mms
                .get_mut(&mm_id)
                .expect("uprobe mm state disappeared");
            state.installed.insert_remapped(
                address,
                InstalledProbe {
                    address,
                    key,
                    original_byte: original,
                    retprobe,
                    plan,
                },
            );
        }
        debug_assert!(published.len() < published.capacity());
        published.push(address);
        let original = match aspace.uprobe_cow_patch_byte(VirtAddr::from(address as usize), 0xcc) {
            Ok(byte) if byte != 0xcc => byte,
            Ok(_) => {
                rollback_prepublished_mapping_install(aspace, mm_id, &patched, &published);
                for (active_key, active_offset) in &activated {
                    let _ =
                        sync_reference_counter_locked(aspace, *active_key, *active_offset, Some(0));
                }
                return Err(AxError::InvalidInput);
            }
            Err(error) => {
                rollback_prepublished_mapping_install(aspace, mm_id, &patched, &published);
                for (active_key, active_offset) in &activated {
                    let _ =
                        sync_reference_counter_locked(aspace, *active_key, *active_offset, Some(0));
                }
                return Err(error);
            }
        };
        if original != decoded[0] {
            if let Some(record) = REGISTRY
                .lock()
                .mms
                .get_mut(&mm_id)
                .and_then(|state| state.installed.get_mut(&address))
            {
                record.original_byte = original;
                record.plan = InstructionPlan::RepairPending;
            }
            debug_assert!(patched.len() < patched.capacity());
            patched.push((address, original, retprobe, plan));
            rollback_prepublished_mapping_install(aspace, mm_id, &patched, &published);
            for (active_key, active_offset) in &activated {
                let _ = sync_reference_counter_locked(aspace, *active_key, *active_offset, Some(0));
            }
            return Err(AxError::BadState);
        }
        debug_assert!(patched.len() < patched.capacity());
        patched.push((address, original, retprobe, plan));
    }
    let _ = patched;
    Ok(())
}

/// Register a perf/trace/BPF consumer.  Publication deliberately precedes
/// overlay installation; a failing installer can unregister this reference
/// without ever exposing an INT3.
pub(crate) fn register(
    file: UprobeFileKey,
    offset: u64,
    retprobe: bool,
    reference_counter_offset: u64,
) -> AxResult<()> {
    if reference_counter_offset & 1 != 0 {
        return Err(AxError::InvalidInput);
    }
    let _topology = registration_topology_gate();
    let key = ProbeKey { file, offset };
    {
        let mut registry = REGISTRY.lock();
        let refs = registry.consumers.entry(key).or_default();
        match refs.declared_reference_counter_offset {
            Some(declared) if declared != reference_counter_offset => {
                return Err(AxError::InvalidInput);
            }
            Some(_) => {}
            None => {}
        }
        let total = refs.total.checked_add(1).ok_or(AxError::NoMemory)?;
        let returns = refs
            .returns
            .checked_add(u32::from(retprobe))
            .ok_or(AxError::NoMemory)?;
        let mut counter_count = None;
        if reference_counter_offset != 0 {
            if let Some(counter) = refs.reference_counters.first() {
                if counter.offset != reference_counter_offset {
                    return Err(AxError::InvalidInput);
                }
                let next = counter.count.checked_add(1).ok_or(AxError::NoMemory)?;
                if next > u16::MAX as u32 {
                    return Err(AxError::InvalidInput);
                }
                counter_count = Some(next);
            } else {
                refs.reference_counters
                    .try_reserve(1)
                    .map_err(|_| AxError::NoMemory)?;
                counter_count = Some(1);
            }
        }
        if refs.declared_reference_counter_offset.is_none() {
            refs.declared_reference_counter_offset = Some(reference_counter_offset);
        }
        if let Some(counter_count) = counter_count {
            if let Some(counter) = refs.reference_counters.first_mut() {
                counter.count = counter_count;
            } else {
                refs.reference_counters.push(ReferenceCounterRefs {
                    offset: reference_counter_offset,
                    count: counter_count,
                });
            }
        }
        refs.total = total;
        refs.returns = returns;
        refs.retiring = false;
    }
    // `REGISTRY.mms` only contains address spaces which have already touched
    // an uprobe or XOL path.  A CPU/cgroup event must instead start from the
    // process-image live-mm registry, otherwise the first system-wide probe
    // silently misses ordinary processes whose executable mapping predates
    // registration.
    let mut address_spaces = crate::mm::live_address_spaces();
    // A CPU/cgroup uprobe has no single target mm. Publication therefore
    // installs the object+offset probe into every already-known mm; later
    // mmap/fork/exec reconciliation observes the same global consumer. Keep
    // this one registration failure-atomic by releasing all installed
    // overlays if any mm cannot complete its COW breakpoint transaction.
    address_spaces
        .retain(|aspace| !matching_probe_addresses(&aspace.lock(), file, offset).is_empty());
    address_spaces.sort_by_key(|aspace| aspace.lock().address_space_id().get());
    for aspace in &address_spaces {
        if let Err(error) = ensure_xol_mapping(aspace) {
            let _ = unregister_metadata(key, retprobe, reference_counter_offset);
            crate::deferred_work::wake_uprobe_restore_worker();
            return Err(error);
        }
    }
    // Lock every participating mm in stable identity order.  Registration is
    // published globally before this point, but no successful mm may expose
    // its extra semaphore contribution unless every currently matching mm can
    // complete the instruction transaction.
    let mut locked = Vec::new();
    for aspace in &address_spaces {
        locked.push(aspace.lock());
    }
    for index in 0..locked.len() {
        let result = install_for_mm_locked(
            &address_spaces[index],
            &mut locked[index],
            file,
            offset,
            retprobe,
            reference_counter_offset,
        );
        if let Err(error) = result {
            let retiring =
                unregister_metadata(key, retprobe, reference_counter_offset).unwrap_or(false);
            for mm in &mut locked {
                let mm_id = mm.address_space_id().get();
                let installed =
                    REGISTRY.lock().mms.get(&mm_id).is_some_and(|state| {
                        state.installed.values().any(|probe| probe.key == key)
                    });
                let _ = sync_reference_counter_locked(
                    mm,
                    key,
                    reference_counter_offset,
                    (!installed).then_some(0),
                );
                if retiring {
                    let probes = REGISTRY
                        .lock()
                        .mms
                        .get(&mm_id)
                        .map(|state| {
                            state
                                .installed
                                .values()
                                .filter(|probe| probe.key == key)
                                .copied()
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    for probe in probes {
                        let _ = restore_if_inactive(mm_id, mm, probe);
                    }
                }
            }
            drop(locked);
            crate::deferred_work::wake_uprobe_restore_worker();
            return Err(error);
        }
    }
    Ok(())
}

fn unregister_metadata(
    key: ProbeKey,
    retprobe: bool,
    reference_counter_offset: u64,
) -> Option<bool> {
    let mut registry = REGISTRY.lock();
    let (return_active, retiring) = {
        let refs = registry.consumers.get_mut(&key)?;
        refs.total = refs.total.saturating_sub(1);
        if retprobe {
            refs.returns = refs.returns.saturating_sub(1);
        }
        if reference_counter_offset != 0
            && let Some(counter) = refs
                .reference_counters
                .iter_mut()
                .find(|counter| counter.offset == reference_counter_offset)
        {
            counter.count = counter.count.saturating_sub(1);
        }
        refs.retiring = refs.total == 0;
        (refs.returns != 0, refs.retiring)
    };
    for mm in registry.mms.values_mut() {
        for probe in mm.installed.values_mut().filter(|probe| probe.key == key) {
            probe.retprobe = return_active;
        }
    }
    Some(retiring)
}

pub(crate) fn unregister(
    file: UprobeFileKey,
    offset: u64,
    retprobe: bool,
    reference_counter_offset: u64,
) {
    let key = ProbeKey { file, offset };
    let Some(retiring) = unregister_metadata(key, retprobe, reference_counter_offset) else {
        return;
    };
    if retiring || reference_counter_offset != 0 {
        // `unregister` is reachable from IRQ-side final Arc destruction.  It
        // only publishes allocation-free custody here; the dedicated
        // task-context owner faults reclaimed COW pages back in and removes
        // the zero-reference registry entry after every alias is restored.
        crate::deferred_work::wake_uprobe_restore_worker();
    }
}

/// Bind a newly installed RX XOL/trampoline mapping to one mm.  The caller is
/// the MM special-mapping transaction, which must map this range RX (never W+X)
/// before it calls us.  A generation is returned so stale exec/unmap commits
/// cannot reuse a prior overlay identity.
pub(crate) fn bind_xol_mapping(
    aspace: &Arc<Mutex<AddrSpace>>,
    base: u64,
    token: u64,
) -> AxResult<u64> {
    if base == 0 || token == 0 || base & 0xfff != 0 {
        return Err(AxError::InvalidInput);
    }
    let guard = aspace.lock();
    if !is_exact_xol_vma(&guard, base, token) {
        return Err(AxError::BadState);
    }
    let mm = guard.address_space_id().get();
    drop(guard);
    let generation = NEXT_OVERLAY_GENERATION.fetch_add(1, Ordering::Relaxed);
    let mut registry = REGISTRY.lock();
    let state = registry.mms.entry(mm).or_default();
    state.xol_base = base;
    state.trampoline_base = base;
    state.xol_generation = generation;
    state.xol_token = token;
    state.aspace = Arc::downgrade(aspace);
    Ok(generation)
}

/// Installs an INT3 through the private file COW overlay before publishing the
/// registry record.  `original_byte` is retained only as a caller consistency
/// check; the authoritative byte is read by the protected MM transaction.
pub(crate) fn publish_installed(
    aspace: &Arc<Mutex<AddrSpace>>,
    address: u64,
    file: UprobeFileKey,
    offset: u64,
    retprobe: bool,
    original_byte: u8,
) -> AxResult<()> {
    let key = ProbeKey { file, offset };
    if !REGISTRY.lock().consumers.contains_key(&key) {
        return Err(AxError::NotFound);
    }
    let (mm, captured, plan) = {
        let mut guard = aspace.lock();
        let mut decoded = [0u8; 15];
        let readable = (PAGE_SIZE_4K - (address as usize & (PAGE_SIZE_4K - 1))).min(15);
        guard.read(VirtAddr::from(address as usize), &mut decoded[..readable])?;
        let captured = decoded[0];
        let plan = plan_from_bytes(address, captured, decoded, readable)?;
        let patched = guard.uprobe_cow_patch_byte(VirtAddr::from(address as usize), 0xcc)?;
        if patched != captured
            || patched == 0xcc
            || (original_byte != 0 && original_byte != patched)
        {
            let _ = guard.uprobe_cow_patch_byte(VirtAddr::from(address as usize), patched);
            return Err(AxError::InvalidInput);
        }
        let mut verify = [0u8; 15];
        guard.read(VirtAddr::from(address as usize), &mut verify[..readable])?;
        verify[0] = captured;
        if verify[..readable] != decoded[..readable] {
            let _ = guard.uprobe_cow_patch_byte(VirtAddr::from(address as usize), captured);
            return Err(AxError::InvalidInput);
        }
        (guard.address_space_id().get(), captured, plan)
    };
    let mut registry = REGISTRY.lock();
    if !registry.consumers.contains_key(&key) {
        drop(registry);
        let _ = patch_overlay(aspace, address, captured);
        return Err(AxError::NotFound);
    }
    let state = registry.mms.entry(mm).or_default();
    state.aspace = Arc::downgrade(aspace);
    state.installed.insert(
        address,
        InstalledProbe {
            address,
            key,
            original_byte: captured,
            retprobe,
            plan,
        },
    );
    Ok(())
}

fn reserve_xol_slot(mm_id: u64, task: u64) -> AxResult<u64> {
    let mut registry = REGISTRY.lock();
    let mm = registry.mms.get_mut(&mm_id).ok_or(AxError::BadState)?;
    let base = mm
        .xol_base
        .checked_add(XOL_SLOT_OFFSET as u64)
        .ok_or(AxError::BadAddress)?;
    for index in 0..XOL_SLOT_COUNT {
        let slot = base + (index * XOL_SLOT_SIZE) as u64;
        if !mm.xol_slots.contains_key(&slot) {
            mm.xol_slots.insert(slot, task);
            return Ok(slot);
        }
    }
    Err(AxError::NoMemory)
}

fn release_xol_slot(mm_id: u64, task: u64, slot: u64) {
    let mut registry = REGISTRY.lock();
    if let Some(mm) = registry.mms.get_mut(&mm_id)
        && mm.xol_slots.get(&slot).is_some_and(|owner| *owner == task)
    {
        mm.xol_slots.remove(&slot);
    }
}

/// Decode the trapped instruction and relocate it into an RX XOL slot.  The
/// original first byte is supplied from the protected install record because
/// the user mapping currently contains INT3.  One-instruction XOL excludes
/// control transfer and privileged/invalid forms; those require Linux's
/// multi-instruction simulation path and must never be executed at an
/// invented PC.
fn materialize_xol(
    aspace: &Arc<Mutex<AddrSpace>>,
    probe: InstalledProbe,
    slot: u64,
) -> AxResult<(u8, u64)> {
    let InstructionPlan::Relocated { bytes, len } = probe.plan else {
        return Err(AxError::Unsupported);
    };
    let mut decoder = Decoder::with_ip(
        64,
        &bytes[..len as usize],
        probe.address,
        DecoderOptions::NONE,
    );
    let instruction = decoder.decode();
    if instruction.is_invalid()
        || instruction.len() == 0
        || instruction.flow_control() != FlowControl::Next
    {
        return Err(AxError::Unsupported);
    }
    let result = BlockEncoder::encode(
        64,
        InstructionBlock::new(core::slice::from_ref(&instruction), slot),
        BlockEncoderOptions::NONE,
    )
    .map_err(|_| AxError::Unsupported)?;
    if result.code_buffer.is_empty() || result.code_buffer.len() >= XOL_SLOT_SIZE {
        return Err(AxError::Unsupported);
    }
    aspace
        .lock()
        .write(VirtAddr::from(slot as usize), &result.code_buffer)?;
    Ok((
        instruction.len() as u8,
        slot + result.code_buffer.len() as u64,
    ))
}

fn condition_holds(condition: ConditionCode, flags: u64) -> bool {
    let cf = flags & 1 != 0;
    let pf = flags & (1 << 2) != 0;
    let zf = flags & (1 << 6) != 0;
    let sf = flags & (1 << 7) != 0;
    let of = flags & (1 << 11) != 0;
    match condition {
        ConditionCode::o => of,
        ConditionCode::no => !of,
        ConditionCode::b => cf,
        ConditionCode::ae => !cf,
        ConditionCode::e => zf,
        ConditionCode::ne => !zf,
        ConditionCode::be => cf || zf,
        ConditionCode::a => !cf && !zf,
        ConditionCode::s => sf,
        ConditionCode::ns => !sf,
        ConditionCode::p => pf,
        ConditionCode::np => !pf,
        ConditionCode::l => sf != of,
        ConditionCode::ge => sf == of,
        ConditionCode::le => zf || sf != of,
        ConditionCode::g => !zf && sf == of,
        _ => false,
    }
}

fn frame_register(frame: &TrapFrame, reg: Register) -> Option<u64> {
    Some(match reg {
        Register::RAX => frame.rax,
        Register::RCX => frame.rcx,
        Register::RDX => frame.rdx,
        Register::RBX => frame.rbx,
        Register::RSP => frame.rsp,
        Register::RBP => frame.rbp,
        Register::RSI => frame.rsi,
        Register::RDI => frame.rdi,
        Register::R8 => frame.r8,
        Register::R9 => frame.r9,
        Register::R10 => frame.r10,
        Register::R11 => frame.r11,
        Register::R12 => frame.r12,
        Register::R13 => frame.r13,
        Register::R14 => frame.r14,
        Register::R15 => frame.r15,
        Register::EAX => frame.rax as u32 as u64,
        Register::ECX => frame.rcx as u32 as u64,
        Register::EDX => frame.rdx as u32 as u64,
        Register::EBX => frame.rbx as u32 as u64,
        Register::ESP => frame.rsp as u32 as u64,
        Register::EBP => frame.rbp as u32 as u64,
        Register::ESI => frame.rsi as u32 as u64,
        Register::EDI => frame.rdi as u32 as u64,
        Register::RIP => frame.rip,
        Register::CS | Register::DS | Register::ES | Register::SS => Some(0)?,
        _ => return None,
    })
}

fn emulate_indirect_control(
    frame: &mut TrapFrame,
    probe: InstalledProbe,
) -> Result<bool, ControlError> {
    let InstructionPlan::ControlledUnsupported { bytes, len } = probe.plan else {
        return Ok(false);
    };
    let mut decoder = Decoder::with_ip(
        64,
        &bytes[..len as usize],
        probe.address,
        DecoderOptions::NONE,
    );
    let instruction = decoder.decode();
    let flow = instruction.flow_control();
    if !matches!(
        flow,
        FlowControl::IndirectBranch | FlowControl::IndirectCall
    ) {
        return Ok(false);
    }
    let target = match instruction.op0_kind() {
        OpKind::Register => {
            frame_register(frame, instruction.op0_register()).ok_or(ControlError::Unsupported)?
        }
        kind if matches!(
            kind,
            OpKind::MemorySegSI
                | OpKind::MemorySegESI
                | OpKind::MemorySegRSI
                | OpKind::MemorySegDI
                | OpKind::MemorySegEDI
                | OpKind::MemorySegRDI
                | OpKind::MemoryESDI
                | OpKind::MemoryESEDI
                | OpKind::MemoryESRDI
                | OpKind::Memory
        ) =>
        {
            let ea = instruction
                .virtual_address(0, 0, |reg, _, _| {
                    // #BP leaves RIP after INT3.  The x86 effective address
                    // base for RIP-relative control operands is the original
                    // instruction's next IP instead.
                    if reg == Register::RIP {
                        Some(probe.address + instruction.len() as u64)
                    } else {
                        frame_register(frame, reg)
                    }
                })
                .ok_or(ControlError::Unsupported)?;
            UserMemoryCapability::new(current_aspace())
                .read_value(ea as *const u64)
                .map_err(|_| ControlError::Memory(ea))?
        }
        _ => return Ok(false),
    };
    if flow == FlowControl::IndirectBranch {
        frame.rip = target;
        return Ok(true);
    }
    let return_ip = probe.address + instruction.len() as u64;
    commit_call_stack(frame, return_ip)?;
    frame.rip = target;
    Ok(true)
}

fn commit_call_stack_with_return(
    frame: &mut TrapFrame,
    pushed_return: u64,
) -> Result<u64, ControlError> {
    let new_rsp = frame
        .rsp
        .checked_sub(8)
        .ok_or(ControlError::Memory(frame.rsp))?;
    let cet_enabled = shadow_stack_enabled();
    let mut cet_commit = None;
    let shadow = if cet_enabled {
        let cet = crate::task::current_user_live_cet_state();
        let next = cet
            .pl3_ssp
            .checked_sub(8)
            .ok_or(ControlError::ControlProtection)?;
        let slot = next.checked_sub(8).ok_or(ControlError::ControlProtection)?;
        cet_commit = Some((cet, next));
        Some(slot)
    } else {
        None
    };
    commit_call_words(new_rsp, pushed_return, shadow)?;
    if let Some((mut cet, next)) = cet_commit {
        cet.pl3_ssp = next;
        crate::task::set_current_user_cet_state(cet);
    }
    frame.rsp = new_rsp;
    Ok(new_rsp)
}

fn commit_call_stack(frame: &mut TrapFrame, return_ip: u64) -> Result<(), ControlError> {
    let _ = commit_call_stack_with_return(frame, return_ip)?;
    Ok(())
}

/// Turn a probed direct CALL into the kernel-owned optimized return path.
/// The callee still executes normally; only the return word is changed to the
/// RX trampoline, and the original continuation is retained in task-owned
/// state.  Thus syscall 336 is never reachable from an arbitrary user jump.
fn arm_optimized_call(frame: &mut TrapFrame, probe: InstalledProbe) -> Result<u64, ControlError> {
    let InstructionPlan::DirectControl {
        flow: FlowControl::Call,
        len,
        ..
    } = probe.plan
    else {
        return Err(ControlError::Unsupported);
    };
    // x86 optimized uprobes use a five-byte CALL return site.  Other direct
    // encodings retain the exact emulation path rather than inventing a
    // continuation address with the wrong ABI.
    if len != 5 {
        return Err(ControlError::Unsupported);
    }
    let aspace = current_aspace();
    let trampoline = ensure_xol_mapping(&aspace).map_err(|_| ControlError::Memory(frame.rsp))?
        + UPROBE_TRAMPOLINE_OFFSET as u64;
    let mm_id = current_mm_id();
    let (generation, token) = REGISTRY
        .lock()
        .mms
        .get(&mm_id)
        .map(|state| (state.xol_generation, state.xol_token))
        .filter(|(generation, token)| *generation != 0 && *token != 0)
        .ok_or(ControlError::Unsupported)?;
    let task = current_task_id();
    {
        let mut threads = THREADS.lock();
        let state = threads.entry(task).or_default();
        if state.pending_uprobe_syscall.is_some() {
            return Err(ControlError::Unsupported);
        }
        // The trampoline saves three words before SYSCALL.  Pre-compute the
        // exact stack identity before mutating the architectural CALL frame.
        let syscall_stack = frame
            .rsp
            .checked_sub(24)
            .ok_or(ControlError::Memory(frame.rsp))?;
        state.pending_uprobe_syscall = Some(PendingUprobeSyscall {
            mm_id,
            xol_generation: generation,
            xol_token: token,
            entry: probe.address,
            return_address: probe.address + len as u64,
            syscall_stack,
            key: probe.key,
            needs_rebind: false,
        });
    }
    match commit_call_stack_with_return(frame, trampoline) {
        Ok(_) => Ok(trampoline),
        Err(error) => {
            THREADS
                .lock()
                .get_mut(&task)
                .and_then(|state| state.pending_uprobe_syscall.take());
            Err(error)
        }
    }
}

fn consume_shadow_return(expected: u64) -> Result<(), ControlError> {
    if !shadow_stack_enabled() {
        return Ok(());
    }
    let mut cet = crate::task::current_user_live_cet_state();
    let shadow = cet
        .pl3_ssp
        .checked_sub(8)
        .ok_or(ControlError::ControlProtection)?;
    let aspace = current_aspace();
    let result = try_user_nofault_transaction(&aspace, |tx| {
        let span = tx
            .pin_read(shadow as usize, 8)
            .map_err(|_| UserNofaultError::BadAddress)?;
        let mut bytes = [0u8; 8];
        tx.read_pinned(&span, &mut bytes);
        if !tx_shadow_stack_word_valid(tx, shadow as usize) || u64::from_ne_bytes(bytes) != expected
        {
            return Ok(Err(ControlError::ControlProtection));
        }
        Ok(Ok(()))
    });
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err(ControlError::ControlProtection),
    }
    cet.pl3_ssp = cet
        .pl3_ssp
        .checked_add(8)
        .ok_or(ControlError::ControlProtection)?;
    crate::task::set_current_user_cet_state(cet);
    Ok(())
}

/// Commit a normal-stack word and, when CET is active, its shadow companion
/// under one nofault MM transaction. Every read and permission/presence probe
/// precedes the first write while the address-space lock remains held; after
/// that point both fixed 8-byte writes are resident translations and cannot
/// fault or observe an unmap/remap halfway through the architectural update.
fn tx_shadow_stack_word_valid(tx: &crate::mm::UserNofaultTransaction<'_>, start: usize) -> bool {
    // Pinning a shadow write is both the exact VMA/PTE check and the
    // transaction-local mapping-generation check; no separate MM lock may
    // observe a different topology.
    tx.pin_shadow_stack_write(start, 8).is_ok()
}

fn commit_return_words(
    normal: u64,
    replacement: u64,
    shadow: Option<u64>,
) -> Result<u64, ControlError> {
    let aspace = current_aspace();
    let result = try_user_nofault_transaction(&aspace, |tx| {
        let normal_read = tx.pin_read(normal as usize, 8)?;
        let normal_write = tx.pin_user_write(normal as usize, 8)?;
        let mut normal_bytes = [0u8; 8];
        tx.read_pinned(&normal_read, &mut normal_bytes);
        let original = u64::from_ne_bytes(normal_bytes);
        let shadow_write = if let Some(shadow) = shadow {
            let shadow_read = match tx.pin_read(shadow as usize, 8) {
                Ok(span) => span,
                Err(_) => return Ok(Err(ControlError::ControlProtection)),
            };
            let shadow_write = match tx.pin_shadow_stack_write(shadow as usize, 8) {
                Ok(span) => span,
                Err(_) => return Ok(Err(ControlError::ControlProtection)),
            };
            let mut shadow_bytes = [0u8; 8];
            tx.read_pinned(&shadow_read, &mut shadow_bytes);
            if u64::from_ne_bytes(shadow_bytes) != original {
                return Ok(Err(ControlError::ControlProtection));
            }
            Some(shadow_write)
        } else {
            None
        };
        // From here all translations and permissions are captured.  These
        // copies cannot fail and therefore need no compensation writes.
        tx.write_pinned(&normal_write, &replacement.to_ne_bytes());
        if let Some(shadow_write) = shadow_write {
            tx.write_pinned(&shadow_write, &replacement.to_ne_bytes());
        }
        Ok(Ok(original))
    });
    match result {
        Ok(Ok(original)) => Ok(original),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(ControlError::Memory(normal)),
    }
}

/// Push the same return address to the ordinary and CET stacks. As with a
/// hardware CALL, neither destination becomes visible until both resident
/// spans have been preflighted under one MM lock.
fn commit_call_words(normal: u64, return_ip: u64, shadow: Option<u64>) -> Result<(), ControlError> {
    let aspace = current_aspace();
    let result = try_user_nofault_transaction(&aspace, |tx| {
        let normal_read = tx.pin_read(normal as usize, 8)?;
        let normal_write = tx.pin_user_write(normal as usize, 8)?;
        let mut discard = [0u8; 8];
        tx.read_pinned(&normal_read, &mut discard);
        let shadow_write = if let Some(shadow) = shadow {
            let shadow_read = match tx.pin_read(shadow as usize, 8) {
                Ok(span) => span,
                Err(_) => return Ok(Err(ControlError::ControlProtection)),
            };
            tx.read_pinned(&shadow_read, &mut discard);
            Some(match tx.pin_shadow_stack_write(shadow as usize, 8) {
                Ok(span) => span,
                Err(_) => return Ok(Err(ControlError::ControlProtection)),
            })
        } else {
            None
        };
        tx.write_pinned(&normal_write, &return_ip.to_ne_bytes());
        if let Some(shadow_write) = shadow_write {
            tx.write_pinned(&shadow_write, &return_ip.to_ne_bytes());
        }
        Ok(Ok(()))
    });
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(ControlError::Memory(normal)),
    }
}

/// Snapshot the architectural RET words under one MM lock.  CET validation,
/// both reads, and the matching check all precede TrapFrame or SSP mutation.
fn staged_return_target(normal: u64, ssp: Option<u64>) -> Result<u64, ControlError> {
    let aspace = current_aspace();
    let result = try_user_nofault_transaction(&aspace, |tx| {
        let normal_span = tx.pin_read(normal as usize, 8)?;
        let mut normal_bytes = [0u8; 8];
        tx.read_pinned(&normal_span, &mut normal_bytes);
        let target = u64::from_ne_bytes(normal_bytes);
        if let Some(ssp) = ssp {
            let Some(shadow) = ssp.checked_sub(8) else {
                return Ok(Err(ControlError::ControlProtection));
            };
            let shadow_span = match tx.pin_read(shadow as usize, 8) {
                Ok(span) => span,
                Err(_) => return Ok(Err(ControlError::ControlProtection)),
            };
            // A read alone is not CET authority.  Pinning the paired kernel
            // write captures the exact SHSTK VMA/PTE under this same lock.
            let _shadow_exact = match tx.pin_shadow_stack_write(shadow as usize, 8) {
                Ok(span) => span,
                Err(_) => return Ok(Err(ControlError::ControlProtection)),
            };
            let mut shadow_bytes = [0u8; 8];
            tx.read_pinned(&shadow_span, &mut shadow_bytes);
            if u64::from_ne_bytes(shadow_bytes) != target {
                return Ok(Err(ControlError::ControlProtection));
            }
        }
        Ok(Ok(target))
    });
    match result {
        Ok(Ok(target)) => Ok(target),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(ControlError::Memory(normal)),
    }
}

fn emulate_direct_control(
    frame: &mut TrapFrame,
    probe: InstalledProbe,
) -> Result<bool, ControlError> {
    if let InstructionPlan::LoopControl {
        mnemonic,
        target,
        len,
    } = probe.plan
    {
        let zf = frame.rflags & (1 << 6) != 0;
        let take = match mnemonic {
            Mnemonic::Jcxz | Mnemonic::Jecxz | Mnemonic::Jrcxz => frame.rcx == 0,
            Mnemonic::Loop => {
                frame.rcx = frame.rcx.wrapping_sub(1);
                frame.rcx != 0
            }
            Mnemonic::Loope => {
                frame.rcx = frame.rcx.wrapping_sub(1);
                frame.rcx != 0 && zf
            }
            Mnemonic::Loopne => {
                frame.rcx = frame.rcx.wrapping_sub(1);
                frame.rcx != 0 && !zf
            }
            _ => return Ok(false),
        };
        frame.rip = if take {
            target
        } else {
            probe.address + len as u64
        };
        return Ok(true);
    }
    let InstructionPlan::DirectControl {
        flow,
        target,
        condition,
        len,
        stack_increment,
    } = probe.plan
    else {
        return Ok(false);
    };
    match flow {
        FlowControl::UnconditionalBranch => {
            frame.rip = target;
            Ok(true)
        }
        FlowControl::ConditionalBranch => {
            frame.rip = if condition_holds(condition, frame.rflags) {
                target
            } else {
                probe.address + len as u64
            };
            Ok(true)
        }
        FlowControl::Call => {
            // A five-byte near CALL has a live optimized trampoline path.
            // If it cannot be armed (for example nested inside an already
            // pending trampoline) retain exact architectural CALL emulation.
            match arm_optimized_call(frame, probe) {
                Ok(_) => {}
                Err(ControlError::Unsupported) => {
                    let return_ip = probe.address + len as u64;
                    commit_call_stack(frame, return_ip)?;
                }
                Err(error) => return Err(error),
            }
            frame.rip = target;
            Ok(true)
        }
        FlowControl::Return => {
            let next_rsp = frame
                .rsp
                .checked_add(stack_increment as u64)
                .ok_or(ControlError::Memory(frame.rsp))?;
            let mut cet_commit =
                shadow_stack_enabled().then(crate::task::current_user_live_cet_state);
            let return_ip =
                staged_return_target(frame.rsp, cet_commit.as_ref().map(|cet| cet.pl3_ssp))?;
            if let Some(cet) = &mut cet_commit {
                cet.pl3_ssp = cet
                    .pl3_ssp
                    .checked_add(8)
                    .ok_or(ControlError::ControlProtection)?;
            }
            // All fallible reads and CET validation completed above. Publish
            // architectural state only at this final edge.
            if let Some(cet) = cet_commit {
                crate::task::set_current_user_cet_state(cet);
            }
            frame.rsp = next_rsp;
            frame.rip = return_ip;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// #BP intercept, invoked before ordinary SIGTRAP handling.  An unclaimed
/// INT3 remains a normal user breakpoint.  Claiming an installed uprobe emits
/// the shared perf event and records a single-step owner; the architecture XOL
/// mapper subsequently redirects execution to its RX slot.
pub(crate) fn breakpoint(frame: &mut TrapFrame) -> BreakpointClaim {
    let address = frame.rip.wrapping_sub(1);
    let mm_id = current_mm_id();
    let (mut probe, mut active) = {
        let registry = REGISTRY.lock();
        let probe = registry
            .mms
            .get(&mm_id)
            .and_then(|mm| mm.installed.get(&address))
            .copied();
        let Some(probe) = probe else {
            return BreakpointClaim::Unowned;
        };
        (
            probe,
            registry
                .consumers
                .get(&probe.key)
                .is_some_and(|refs| refs.total != 0),
        )
    };
    if !active {
        // Final-close restoration can be waiting for a reclaimed COW page.
        // The trapped thread is now a safe task-context retry owner.  If the
        // byte is restored, rewind RIP so the original instruction executes
        // normally; if memory is still unavailable, execute one XOL copy but
        // never emit a retired perf event or create a return instance.
        let aspace = current_aspace();
        match restore_trapped_if_inactive(mm_id, &aspace, probe) {
            Ok(true) => {}
            Ok(false) => return BreakpointClaim::Unowned,
            Err(error) => {
                error!("resident uprobe retirement restore failed: {error}");
                // Returning `false` would misdeliver the kernel-owned stale
                // INT3 as SIGTRAP. The fetched leaf and registry record
                // disagree, so continuing would violate instruction-stream
                // integrity.
                axhal::power::system_off();
            }
        }
        let registry = REGISTRY.lock();
        let still_installed = registry
            .mms
            .get(&mm_id)
            .and_then(|mm| mm.installed.get(&address))
            .is_some_and(|installed| installed.key == probe.key);
        if !still_installed {
            frame.rip = address;
            return BreakpointClaim::Claimed;
        }
        active = registry
            .consumers
            .get(&probe.key)
            .is_some_and(|refs| refs.total != 0);
        probe.retprobe &= active;
    }
    let task = current_task_id();
    if THREADS
        .lock()
        .get(&task)
        .is_some_and(|state| state.pending_xol.is_some())
    {
        // A nested breakpoint while the saved instruction is executing is not
        // ours; preserving it avoids consuming a debugger's SIGTRAP.
        return BreakpointClaim::Unowned;
    }
    if matches!(probe.plan, InstructionPlan::RepairPending) {
        return claim_xol_failure(frame, address);
    }
    if matches!(probe.plan, InstructionPlan::ControlledUnsupported { .. }) {
        if active {
            publish_entry_hit(frame, probe, task);
        }
        match emulate_indirect_control(frame, probe) {
            Ok(true) => return BreakpointClaim::Claimed,
            Ok(false) => return claim_xol_failure(frame, address),
            Err(ControlError::Unsupported) => return claim_xol_failure(frame, address),
            Err(error) => {
                signal_probe_control_error(frame, address, error);
                return BreakpointClaim::Claimed;
            }
        }
    }
    if matches!(
        probe.plan,
        InstructionPlan::DirectControl { .. } | InstructionPlan::LoopControl { .. }
    ) {
        if active {
            publish_entry_hit(frame, probe, task);
        }
        let prepared = match prepare_return_instance(frame, probe) {
            Ok(prepared) => prepared,
            Err(ControlError::Unsupported) => return claim_xol_failure(frame, address),
            Err(error) => {
                signal_probe_control_error(frame, address, error);
                return BreakpointClaim::Claimed;
            }
        };
        match emulate_direct_control(frame, probe) {
            Ok(true) => return BreakpointClaim::Claimed,
            Ok(false) => {
                if let Err(rollback) = rollback_return_instance(probe, prepared) {
                    signal_probe_control_error(frame, address, rollback);
                    return BreakpointClaim::Claimed;
                }
                return claim_xol_failure(frame, address);
            }
            Err(error) => {
                if let Err(rollback) = rollback_return_instance(probe, prepared) {
                    signal_probe_control_error(frame, address, rollback);
                } else if matches!(error, ControlError::Unsupported) {
                    return claim_xol_failure(frame, address);
                } else {
                    signal_probe_control_error(frame, address, error);
                }
                return BreakpointClaim::Claimed;
            }
        }
    }
    let aspace = current_aspace();
    if ensure_xol_mapping(&aspace).is_err() {
        return claim_xol_failure(frame, address);
    }
    let slot = match reserve_xol_slot(mm_id, task) {
        Ok(slot) => slot,
        Err(_) => return claim_xol_failure(frame, address),
    };
    let (instruction_len, xol_end) = match materialize_xol(&aspace, probe, slot) {
        Ok(xol) => xol,
        Err(_) => {
            release_xol_slot(mm_id, task, slot);
            return claim_xol_failure(frame, address);
        }
    };
    let identity = REGISTRY
        .lock()
        .mms
        .get(&mm_id)
        .map(|mm| (mm.xol_generation, mm.xol_token))
        .filter(|(generation, token)| *generation != 0 && *token != 0);
    let Some((generation, token)) = identity else {
        release_xol_slot(mm_id, task, slot);
        return claim_xol_failure(frame, address);
    };
    // The original page remains INT3 throughout execution.  This is the key
    // distinction from a TF-at-original-PC fallback: another thread can
    // never run a temporarily restored byte.
    let prepared = match prepare_return_instance(frame, probe) {
        Ok(prepared) => prepared,
        Err(ControlError::Unsupported) => {
            release_xol_slot(mm_id, task, slot);
            return claim_xol_failure(frame, address);
        }
        Err(error) => {
            release_xol_slot(mm_id, task, slot);
            signal_probe_control_error(frame, address, error);
            return BreakpointClaim::Claimed;
        }
    };
    let mut threads = THREADS.lock();
    let state = threads.entry(task).or_default();
    if state.pending_xol.is_some() {
        drop(threads);
        if let Err(error) = rollback_return_instance(probe, prepared) {
            signal_probe_control_error(frame, address, error);
            release_xol_slot(mm_id, task, slot);
            return BreakpointClaim::Claimed;
        }
        release_xol_slot(mm_id, task, slot);
        return claim_xol_failure(frame, address);
    }
    state.pending_xol = Some(PendingXol {
        mm_id,
        xol_generation: generation,
        xol_token: token,
        slot,
        expected_end: xol_end,
        original_tf: frame.rflags & (1 << 8) != 0,
        probe,
        aspace,
        resume: probe.address + instruction_len as u64,
    });
    drop(threads);
    if active {
        publish_entry_hit(frame, probe, task);
    }
    // Execute the relocated instruction once, then #DB fixes the architectural
    // PC back to the original post-instruction address.
    frame.rip = slot;
    frame.rflags |= 1 << 8;
    BreakpointClaim::Claimed
}

fn xol_generation_is_live(pending: &PendingXol) -> bool {
    let mm = pending.aspace.lock();
    if mm.address_space_id().get() != pending.mm_id {
        return false;
    }
    let registry = REGISTRY.lock();
    registry.mms.get(&pending.mm_id).is_some_and(|state| {
        state.xol_generation == pending.xol_generation
            && state.xol_token == pending.xol_token
            && is_exact_xol_vma(&mm, state.xol_base, state.xol_token)
    })
}

/// Consume a pending XOL ownership record exactly once.  Rewinding is used
/// before a signal frame or fault is exposed; preserving RIP is used for an
/// unexpected #DB so the ordinary debug exception still observes its PC.
fn terminate_xol(task: u64, frame: Option<&mut TrapFrame>, rewind: bool) -> bool {
    let pending = THREADS
        .lock()
        .get_mut(&task)
        .and_then(|state| state.pending_xol.take());
    let Some(pending) = pending else {
        return false;
    };
    if let Some(frame) = frame {
        if pending.original_tf {
            frame.rflags |= 1 << 8;
        } else {
            frame.rflags &= !(1 << 8);
        }
        if rewind {
            frame.rip = pending.probe.address;
        }
    }
    release_xol_slot(pending.mm_id, task, pending.slot);
    true
}

/// Retire a pending optimized-CALL authorization once control has escaped the
/// callee's stack frame.  This is invoked at signal/control-flow boundaries;
/// a signal delivered *inside* the callee keeps the pending return alive,
/// whereas sigreturn/ptrace/longjmp restored above the saved caller RSP loses
/// the one-shot syscall-336 authority.
pub(crate) fn retire_stale_optimized_call(frame: &TrapFrame) {
    let task = current_task_id();
    let mm_id = current_mm_id();
    let pending = THREADS
        .lock()
        .get(&task)
        .and_then(|state| state.pending_uprobe_syscall);
    let Some(pending) = pending else {
        return;
    };
    let caller_rsp = pending.syscall_stack.saturating_add(24);
    let live = !pending.needs_rebind
        && pending.mm_id == mm_id
        && frame.rsp <= caller_rsp
        && REGISTRY.lock().mms.get(&mm_id).is_some_and(|mm| {
            mm.xol_generation == pending.xol_generation && mm.xol_token == pending.xol_token
        });
    if !live {
        if let Some(state) = THREADS.lock().get_mut(&task)
            && state.pending_uprobe_syscall == Some(pending)
        {
            state.pending_uprobe_syscall.take();
        }
    }
}

/// #DB intercept. It claims only a live, matching DR6.BS XOL completion.
/// A simultaneous watchpoint is deliberately not claimed here; its DR6 bits
/// remain for the perf/debugger path in `perf_sources`.
pub(crate) fn debug(frame: &mut TrapFrame, dr6: u64) -> bool {
    // DR6.BS is the only debug reason owned by our TF single-step.  Hardware
    // watchpoints and debugger state remain visible to their normal handler.
    if dr6 & (1 << 14) == 0 {
        return false;
    }
    let task = current_task_id();
    let pending = THREADS
        .lock()
        .get(&task)
        .and_then(|state| state.pending_xol.clone());
    let Some(pending) = pending else {
        return false;
    };
    if !xol_generation_is_live(&pending) {
        // The XOL VMA was replaced or unmapped. Preserve this #DB as a real
        // exception but release our ownership and restore the caller's TF.
        terminate_xol(task, Some(frame), false);
        return false;
    }
    if frame.rip != pending.expected_end {
        // Do not turn an unexpected XOL #DB into a successful probe hit.
        // Releasing the slot/TF ownership while retaining RIP lets normal
        // debug/signal handling report the actual exception.
        terminate_xol(task, Some(frame), false);
        return false;
    }
    let was_single_stepping = pending.original_tf;
    let _ = terminate_xol(task, Some(frame), false);
    frame.rip = pending.resume;
    // The caller's TF predates this uprobe.  Preserve its #DB at the logical
    // post-instruction PC for ptrace/debugger delivery rather than consuming
    // it as our XOL completion (the next instruction must not become the
    // debugger's observed single-step boundary).
    !was_single_stepping
}

/// Translate a fault, signal-frame diversion, or ptrace-like interrupted XOL
/// execution back to the original instruction address. The original mapping
/// was never unpatched, so retrying it naturally re-enters #BP; no stale XOL
/// byte can leak into a resumed task.
pub(crate) fn abort_xol(frame: &mut TrapFrame) {
    let _ = terminate_xol(current_task_id(), Some(frame), true);
    retire_stale_optimized_call(frame);
}

/// Exec replaces the mm and cannot retain any executable overlay or return
/// address from the old image.
pub(crate) fn on_exec(task_id: u64, old_aspace: &Arc<Mutex<AddrSpace>>) {
    let old_mm_id = old_aspace.lock().address_space_id().get();
    let _ = terminate_xol(task_id, None, false);
    THREADS.lock().remove(&task_id);
    REGISTRY.lock().mms.remove(&old_mm_id);
}

pub(crate) fn on_exit(task_id: u64) {
    let _ = terminate_xol(task_id, None, false);
    THREADS.lock().remove(&task_id);
}

/// Fork duplicates pending return instances, but never a live XOL step: the
/// child begins at the fork return point, not half way through an instruction
/// displaced in its parent.  CLONE_VM safely shares the mm registry while a
/// normal fork gets its own MM overlay transaction at the VMA clone boundary.
pub(crate) fn on_fork(parent_task: u64, child_task: u64) {
    let mut threads = THREADS.lock();
    let Some(parent) = threads.get(&parent_task) else {
        return;
    };
    let mut child = parent.clone();
    child.pending_xol = None;
    if let Some(pending) = &mut child.pending_uprobe_syscall {
        pending.needs_rebind = true;
    }
    threads.insert(child_task, child);
}

/// A non-CLONE_VM fork COW-copies the executable overlay bytes.  Mirror the
/// immutable probe records into the child mm before it is published, while a
/// shared-mm clone simply keeps the one existing record set.
pub(crate) fn on_fork_mm(
    parent_aspace: &Arc<Mutex<AddrSpace>>,
    child_aspace: &Arc<Mutex<AddrSpace>>,
) {
    let _topology = registration_topology_gate();
    // Legacy topology callers which do not own a concrete child task cannot
    // safely rebind an armed optimized return.  The clone admission path uses
    // the task-aware variant below.
    on_fork_mm_gated_for_task(parent_aspace, child_aspace, None);
}

/// Fork construction acquires the topology gate before cloning/registering
/// the pending child mm and retains it through this metadata publication.
pub(crate) fn on_fork_mm_gated(
    parent_aspace: &Arc<Mutex<AddrSpace>>,
    child_aspace: &Arc<Mutex<AddrSpace>>,
    child_task: u64,
) {
    on_fork_mm_gated_for_task(parent_aspace, child_aspace, Some(child_task));
}

fn on_fork_mm_gated_for_task(
    parent_aspace: &Arc<Mutex<AddrSpace>>,
    child_aspace: &Arc<Mutex<AddrSpace>>,
    child_task: Option<u64>,
) {
    let parent_id = parent_aspace.lock().address_space_id().get();
    let child_id = child_aspace.lock().address_space_id().get();
    if Arc::ptr_eq(parent_aspace, child_aspace) {
        // CLONE_VM keeps the existing overlay identity; the copied child is
        // nevertheless no longer waiting for a private-mm rebind.
        if let Some(child_task) = child_task
            && let Some(state) = THREADS.lock().get_mut(&child_task)
            && let Some(pending) = &mut state.pending_uprobe_syscall
            && pending.needs_rebind
            && pending.mm_id == parent_id
        {
            pending.needs_rebind = false;
        }
        return;
    }
    let mut registry = REGISTRY.lock();
    let Some(parent) = registry.mms.get(&parent_id) else {
        return;
    };
    let child = MmProbeState {
        installed: parent.installed.clone(),
        reference_counters: parent.reference_counters.clone(),
        blocked_reference_counters: parent.blocked_reference_counters.clone(),
        xol_base: parent.xol_base,
        trampoline_base: parent.trampoline_base,
        xol_generation: parent.xol_generation,
        xol_token: parent.xol_token,
        aspace: Arc::downgrade(child_aspace),
        xol_slots: BTreeMap::new(),
        pending_mmap_reconcile: parent.pending_mmap_reconcile,
    };
    registry.mms.insert(child_id, child);
    let identity = registry
        .mms
        .get(&child_id)
        .map(|state| (state.xol_generation, state.xol_token));
    drop(registry);
    if let Some((generation, token)) = identity
        && let Some(child_task) = child_task
        && let Some(state) = THREADS.lock().get_mut(&child_task)
        && let Some(pending) = &mut state.pending_uprobe_syscall
        && pending.needs_rebind
        && pending.mm_id == parent_id
    {
        pending.mm_id = child_id;
        pending.xol_generation = generation;
        pending.xol_token = token;
        pending.needs_rebind = false;
    }
}

/// Linux x86-64 `sys_uretprobe`: this entry is reachable only from the
/// kernel-owned RX trampoline.  All malformed/external calls are SIGILL/-1.
pub(crate) fn syscall_uretprobe(frame: &mut TrapFrame) -> AxResult<isize> {
    let mm_id = current_mm_id();
    let task = current_task_id();
    // push rax; push rcx; push r11; mov imm32,rax; syscall
    let allowed = is_live_trampoline_syscall_ip(mm_id, frame.rip, 13);
    if !allowed {
        force_signal_current_thread(SignalInfo::new_kernel(Signo::SIGILL));
        return Ok(-1);
    }
    let Some(return_stack) = frame.rsp.checked_add(16) else {
        force_signal_current_thread(SignalInfo::new_kernel(Signo::SIGILL));
        return Ok(-1);
    };
    // RP_CHECK_RET first discards entries below the current frame (inner
    // returns bypassed by longjmp/signal unwind), then the remaining top
    // entries at this slot form the only legal tail-call chain.
    let instances = {
        let mut threads = THREADS.lock();
        if let Some(state) = threads.get_mut(&task) {
            state
                .returns
                .retain(|instance| instance.stack >= return_stack);
            let mut chain = ReturnChain::new();
            for instance in state.returns.iter().rev() {
                if instance.stack == return_stack {
                    if !chain.push(*instance) {
                        return Ok(-1);
                    }
                } else if !chain.is_empty() {
                    break;
                }
            }
            (!chain.is_empty()).then_some(chain)
        } else {
            None
        }
    };
    let trampoline_base = REGISTRY
        .lock()
        .mms
        .get(&mm_id)
        .map(|state| state.trampoline_base);
    let frame_owned = instances.as_ref().is_some_and(|instances| {
        !instances.is_empty()
            && instances.iter().all(|instance| {
                instance.trampoline != 0 && trampoline_base == Some(instance.trampoline)
            })
    });
    if !frame_owned {
        force_signal_current_thread(thekernel_linux_signal::SignalInfo::new_kernel(
            thekernel_linux_signal::Signo::SIGILL,
        ));
        return Ok(-1);
    }
    // A return trampoline at a higher stack address proves that any pending
    // optimized-call return below it was skipped by longjmp/signal unwinding.
    // Retire that authorization before returning to userspace so a later
    // jump into the RX page cannot reuse an abandoned syscall-336 token.
    if let Some(state) = THREADS.lock().get_mut(&task)
        && state
            .pending_uprobe_syscall
            .is_some_and(|pending| pending.syscall_stack < frame.rsp)
    {
        state.pending_uprobe_syscall.take();
    }
    let instances = instances.expect("validated live uretprobe return chain");
    let final_return = instances
        .last()
        .expect("nonempty checked return chain")
        .original_return;
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    struct Saved {
        r11: u64,
        rcx: u64,
        rax: u64,
    }
    let aspace = current_aspace();
    let saved = match try_user_nofault_transaction(&aspace, |tx| {
        let r11_span = tx.pin_read(frame.rsp as usize, 8)?;
        let rcx_address = frame
            .rsp
            .checked_add(8)
            .ok_or(UserNofaultError::BadAddress)?;
        let rax_address = frame
            .rsp
            .checked_add(16)
            .ok_or(UserNofaultError::BadAddress)?;
        let rcx_span = tx.pin_read(rcx_address as usize, 8)?;
        let rax_span = tx.pin_read(rax_address as usize, 8)?;
        let mut r11 = [0u8; 8];
        let mut rcx = [0u8; 8];
        let mut rax = [0u8; 8];
        tx.read_pinned(&r11_span, &mut r11);
        tx.read_pinned(&rcx_span, &mut rcx);
        tx.read_pinned(&rax_span, &mut rax);
        Ok(Saved {
            r11: u64::from_ne_bytes(r11),
            rcx: u64::from_ne_bytes(rcx),
            rax: u64::from_ne_bytes(rax),
        })
    }) {
        Ok(value) => value,
        Err(_) => {
            force_signal_current_thread(thekernel_linux_signal::SignalInfo::new_kernel(
                thekernel_linux_signal::Signo::SIGILL,
            ));
            return Ok(-1);
        }
    };
    // Commit the liveness walk only after the RX provenance and saved-frame
    // usercopy have both succeeded.  Lower slots are frames skipped by a
    // non-local unwind; equal slots form the tail-call chain delivered here.
    let consumed = {
        let stack = return_stack;
        let mut threads = THREADS.lock();
        let state = threads.get_mut(&task).expect("snapshotted uretprobe state");
        state.returns.retain(|instance| instance.stack >= stack);
        let mut consumed = ReturnChain::new();
        while state
            .returns
            .last()
            .is_some_and(|instance| instance.stack == stack)
        {
            if !consumed.push(state.returns.pop().expect("checked return instance")) {
                return Ok(-1);
            }
        }
        consumed
    };
    if consumed != instances {
        force_signal_current_thread(SignalInfo::new_kernel(Signo::SIGILL));
        return Ok(-1);
    }
    // Linux exposes the trampoline's saved register image to its consumers
    // with SP advanced over all three words before deciding whether to resume
    // directly (CET/consumer-modified SP) or reconstruct the RX stub frame.
    let syscall_ip = frame.rip;
    let syscall_sp = match frame.rsp.checked_add(core::mem::size_of::<Saved>() as u64) {
        Some(sp) => sp,
        None => {
            force_signal_current_thread(SignalInfo::new_kernel(Signo::SIGILL));
            return Ok(-1);
        }
    };
    frame.r11 = saved.r11;
    frame.rcx = saved.rcx;
    frame.rax = saved.rax;
    frame.rsp = syscall_sp;
    // The hardware RET which entered this trampoline has already consumed
    // its SHSTK word.  In particular sys_uretprobe must not pop it again:
    // doing so corrupts the caller's shadow-stack nesting.
    frame.rip = final_return;
    for instance in instances.iter() {
        handle_uprobe_trampoline(instance.key, true, frame);
    }
    let direct_return =
        shadow_stack_enabled() || frame.rsp != syscall_sp || frame.rip != final_return;
    if direct_return {
        for instance in instances.iter().copied() {
            let return_active = REGISTRY
                .lock()
                .consumers
                .get(&instance.key)
                .is_some_and(|refs| refs.total != 0 && refs.returns != 0);
            publish_return_hit(instance, task, return_active);
        }
        return Ok(frame.rax as isize);
    }
    let write_result = try_user_nofault_transaction(&aspace, |tx| {
        let rewrite_sp = syscall_sp
            .checked_sub(core::mem::size_of::<Saved>() as u64)
            .ok_or(UserNofaultError::BadAddress)?;
        let r11_address = rewrite_sp;
        let rcx_address = rewrite_sp
            .checked_add(8)
            .ok_or(UserNofaultError::BadAddress)?;
        let rax_address = rewrite_sp
            .checked_add(16)
            .ok_or(UserNofaultError::BadAddress)?;
        let r11_write = tx.pin_user_write(r11_address as usize, 8)?;
        let rcx_write = tx.pin_user_write(rcx_address as usize, 8)?;
        let rax_read = tx.pin_read(rax_address as usize, 8)?;
        let rax_write = tx.pin_user_write(rax_address as usize, 8)?;
        let mut before = [0u8; 8];
        tx.read_pinned(&rax_read, &mut before);
        if u64::from_ne_bytes(before) != saved.rax {
            return Err(UserNofaultError::BadAddress);
        }
        tx.write_pinned(&r11_write, &frame.r11.to_ne_bytes());
        tx.write_pinned(&rcx_write, &frame.rcx.to_ne_bytes());
        tx.write_pinned(&rax_write, &final_return.to_ne_bytes());
        Ok(())
    });
    if write_result.is_err() {
        force_signal_current_thread(thekernel_linux_signal::SignalInfo::new_kernel(
            thekernel_linux_signal::Signo::SIGILL,
        ));
        return Ok(-1);
    }
    // The RX stub pops r11/rcx and RETs through the rewritten rax slot.
    // Syscall RAX must retain the interrupted function value, not the return
    // address used by RET.
    frame.rsp = syscall_sp - core::mem::size_of::<Saved>() as u64;
    frame.rip = syscall_ip;
    for instance in instances.iter().copied() {
        let return_active = REGISTRY
            .lock()
            .consumers
            .get(&instance.key)
            .is_some_and(|refs| refs.total != 0 && refs.returns != 0);
        publish_return_hit(instance, task, return_active);
    }
    Ok(frame.rax as isize)
}

/// Linux x86-64 `sys_uprobe`: only the special RX trampoline mapping may call
/// it.  Unlike 335, an ordinary external invocation is a plain ENXIO.
pub(crate) fn syscall_uprobe(frame: &mut TrapFrame) -> AxResult<isize> {
    let mm_id = current_mm_id();
    let task = current_task_id();
    let allowed =
        is_live_trampoline_syscall_ip(mm_id, frame.rip, UPROBE_TRAMPOLINE_OFFSET as u64 + 13);
    let pending = THREADS
        .lock()
        .get(&task)
        .and_then(|state| state.pending_uprobe_syscall);
    let live_identity = pending.is_some_and(|pending| {
        !pending.needs_rebind
            && pending.mm_id == mm_id
            && pending.syscall_stack == frame.rsp
            && REGISTRY.lock().mms.get(&mm_id).is_some_and(|state| {
                state.xol_generation == pending.xol_generation
                    && state.xol_token == pending.xol_token
            })
    });
    if !allowed || !live_identity {
        return Err(AxError::from(LinuxError::ENXIO));
    }
    let pending = pending.expect("validated live uprobe syscall invocation");
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    struct Saved {
        rax: u64,
        r11: u64,
        rcx: u64,
    }
    let aspace = current_aspace();
    let saved = match try_user_nofault_transaction(&aspace, |tx| {
        let span = tx.pin_read(frame.rsp as usize, core::mem::size_of::<Saved>())?;
        let mut bytes = [0u8; core::mem::size_of::<Saved>()];
        tx.read_pinned(&span, &mut bytes);
        Ok(bytemuck::pod_read_unaligned::<Saved>(&bytes))
    }) {
        Ok(value) => value,
        _ => {
            THREADS
                .lock()
                .get_mut(&task)
                .and_then(|state| state.pending_uprobe_syscall.take());
            force_signal_current_thread(thekernel_linux_signal::SignalInfo::new_kernel(
                thekernel_linux_signal::Signo::SIGILL,
            ));
            return Ok(-1);
        }
    };
    // `pending` is created only while emulating a live five-byte CALL.  The
    // original continuation is kernel state, never a fourth user-stack word,
    // so a callee cannot forge a continuation by changing the ordinary stack.
    let entry = pending.entry;
    THREADS
        .lock()
        .get_mut(&task)
        .and_then(|state| state.pending_uprobe_syscall.take());
    // The #BP owner already emitted the entry event.  This is the mutable
    // trampoline phase: present the restored registers at the CALL site and
    // honor a consumer's IP/SP/RAX/RCX/R11 edits when returning to userspace.
    frame.rax = saved.rax;
    frame.r11 = saved.r11;
    frame.rcx = saved.rcx;
    frame.rsp = frame
        .rsp
        .checked_add(core::mem::size_of::<Saved>() as u64)
        .ok_or(AxError::BadAddress)?;
    frame.rip = entry;
    handle_uprobe_trampoline(pending.key, false, frame);
    if frame.rip == entry {
        frame.rip = pending.return_address;
    }
    // The callee's hardware RET already consumed the normal and, when
    // enabled, shadow trampoline entries.  Return directly rather than
    // reconstructing a writable RX stack frame; this preserves CET nesting
    // and also honors a consumer-selected stack pointer.
    Ok(frame.rax as isize)
}
