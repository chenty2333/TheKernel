//! `task::process` subsections; see the parent `mod.rs` for the module map.

use super::*;

pub(crate) const UTS_FIELD_LEN: usize = 64;
const PROC_NS_INO_BASE: u64 = 0x9_0000_0000;
static PROC_NS_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn try_allocate_namespace_id(counter: &AtomicU64) -> AxResult<u64> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .map_err(|_| axerrno::LinuxError::ENOSPC.into())
}

fn try_allocate_proc_namespace_id() -> AxResult<u64> {
    try_allocate_namespace_id(&PROC_NS_ID)
}

/// Implementation ceiling for queued RT nodes charged to one (user_ns, ruid).
/// RLIMIT_SIGPENDING may lower this value but cannot raise it.
pub(crate) const SIGNAL_QUEUE_PER_USER_HARD_LIMIT: usize = 4_096;
/// Implementation ceiling for all queued RT nodes in one root user-namespace
/// hierarchy. Descendant NEWUSER namespaces share this account.
pub(crate) const SIGNAL_QUEUE_GLOBAL_HARD_LIMIT: usize = 16_384;
/// Hard ceiling for simultaneously retained user namespaces. Namespace fds
/// can outlive their creator, so RLIMIT_NPROC alone is not a lifetime bound.
pub(crate) const USER_NAMESPACE_HARD_LIMIT: usize = 4_096;
/// Maximum number of live or publication-reserved reverse ptrace links owned
/// by one tracer process.
pub(crate) const PTRACE_REVERSE_LINK_HARD_LIMIT: usize = 4_096;
static LIVE_USER_NAMESPACES: AtomicUsize = AtomicUsize::new(0);

/// Stable identity for one user-namespace object.
///
/// The identifier is allocated once and never reused. Consumers that need to
/// namespace internal state can therefore use this value without retaining an
/// `Arc<UserNamespace>` and extending the namespace lifetime.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct UserNamespaceId(u64);

impl UserNamespaceId {
    /// Stable scalar form for bounded kernel-owned accounting tables.  IDs
    /// are allocated once and never reused, so retaining this value does not
    /// extend the namespace lifetime or permit an ABA match.
pub(crate) const fn into_raw(self) -> u64 {
        self.0
    }
}

pub(crate) fn try_increment_bounded(counter: &AtomicUsize, limit: usize) -> bool {
    counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < limit).then_some(current + 1)
        })
        .is_ok()
}

struct UserNamespaceAdmission;

impl UserNamespaceAdmission {
    fn try_new() -> AxResult<Self> {
        if try_increment_bounded(&LIVE_USER_NAMESPACES, USER_NAMESPACE_HARD_LIMIT) {
            Ok(Self)
        } else {
            Err(axerrno::LinuxError::ENOSPC.into())
        }
    }
}

impl Drop for UserNamespaceAdmission {
    fn drop(&mut self) {
        LIVE_USER_NAMESPACES.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Clone)]
pub(crate) struct CgroupNamespace {
    id: u64,
    owner_user_ns: Arc<UserNamespace>,
    /// The cgroup roots which were visible when this namespace was created.
    ///
    /// A cgroup namespace is not a second hierarchy: it is an immutable view
    /// rooted at the creator's live membership.  Retaining the opaque roots
    /// here keeps that view alive even if the task subsequently migrates, and
    /// lets the cgroup filesystem apply the same root to proc rendering and
    /// pathname visibility after setns().
    roots: crate::pseudofs::cgroup::CgroupNamespaceRoots,
}

/// Namespace-local mount attachment identity.
///
/// Mount topology remains owned by `crate::mounts`; this object is the
/// process-visible namespace handle and the hand-off point for the topology
/// implementation.  Keeping the identity separate from the global VFS
/// mechanisms lets clone/unshare/setns prepare an attach without publishing a
/// partially changed task namespace set.
pub(crate) struct MountNamespace {
    id: u64,
    owner_user_ns: Arc<UserNamespace>,
    topology: Arc<crate::mounts::MountTopology>,
    // FUSE/NFS mount-ID registrations are external provider state.  Keep the
    // clone-owned IDs with the namespace lifetime so dropping the final task
    // or nsfd cannot strand a provider registration after namespace teardown.
    provider_registrations: Mutex<Vec<crate::mounts::ClonedProviderMount>>,
}

/// Namespace IDs in statmount/listmount are references to live namespace
/// objects, not an alias for the caller's current mount graph.  Keep only
/// weak entries: `/proc/*/ns/mnt` and tasks remain the lifetime authority.
static MOUNT_NAMESPACE_REGISTRY: Lazy<Mutex<HashMap<u64, Weak<MountNamespace>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

impl MountNamespace {
pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        let id = try_allocate_proc_namespace_id()?;
        let topology = crate::mounts::MountTopology::try_bootstrap(id)?;
        let namespace = Arc::try_new(Self {
            id,
            owner_user_ns,
            topology,
            provider_registrations: Mutex::new(Vec::new()),
        })
        .map_err(|_| AxError::NoMemory)?;
        Self::register(&namespace)?;
        Ok(namespace)
    }

    /// Forks this namespace's mounts into a new mount namespace.
    ///
    /// `empty` asks for `CLONE_EMPTY_MNTNS`: the new namespace holds only a
    /// clone of the namespace root and no submounts (`fs/namespace.c`:
    /// 4258-4271), so a task that enters it cannot reach `/proc`, `/sys` or the
    /// mutable rootfs until it mounts something itself.
pub(crate) fn try_fork(
        &self,
        owner_user_ns: Arc<UserNamespace>,
        empty: bool,
    ) -> AxResult<Arc<Self>> {
        let id = try_allocate_proc_namespace_id()?;
        // A CLONE_NEWNS/UNSHARE_NEWNS paired with a new user namespace must
        // retain the copied mounts but lock their placement-sensitive state.
        // This is Linux's `lock_mnt_tree()` restriction, not a move_mount
        // policy bit; it therefore belongs to topology construction and is
        // preserved by later namespace clones.
        let lock_mounts = !Arc::ptr_eq(&self.owner_user_ns, &owner_user_ns);
        let mut topology = self
            .topology
            .try_prepare_clone_namespace(id, lock_mounts, empty)?;
        let namespace = Arc::try_new(Self {
            id,
            owner_user_ns,
            topology: topology.topology(),
            provider_registrations: Mutex::new(Vec::new()),
        })
        .map_err(|_| AxError::NoMemory)?;
        // Provider registrations are external state.  Do not activate them
        // until the clone has an owned namespace object; if either this
        // activation or nsfs registry admission fails, the prepared clone's
        // Drop receipt removes every FUSE/NFS registration before the private
        // topology is released.
        topology.activate_provider_mounts()?;
        if let Err(error) = Self::register(&namespace) {
            return Err(error);
        }
        // No fallible work remains after registry admission. Transfer the
        // active registrations into the namespace lifetime before the
        // prepared receipt is dropped.
        *namespace.provider_registrations.lock() = topology.take_active_provider_mounts();
        Ok(namespace)
    }

pub(crate) const fn id(&self) -> u64 {
        self.id
    }
pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }
pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }
pub(crate) fn topology(&self) -> Arc<crate::mounts::MountTopology> {
        self.topology.clone()
    }

    /// The mount a task rooted in this namespace sees at `"/"`, i.e. Linux
    /// `vfs_path_lookup(mnt_ns->root, "/", LOOKUP_DOWN, &root)`
    /// (`fs/namespace.c`:3699-3702).  This is the mutable rootfs, not the
    /// immutable nullfs that owns `ns->root`.
pub(crate) fn root_location(&self) -> AxResult<axfs_ng_vfs::Location> {
        self.topology.visible_root_location()
    }

    fn register(namespace: &Arc<Self>) -> AxResult<()> {
        let mut namespaces = MOUNT_NAMESPACE_REGISTRY.lock();
        namespaces.retain(|_, entry| entry.strong_count() != 0);
        namespaces.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        namespaces.insert(namespace.id(), Arc::downgrade(namespace));
        Ok(())
    }

    /// Resolves the stable namespace ID exposed by statmount/listmount and
    /// NS_GET_ID.  This is deliberately distinct from the nsfs inode number
    /// used by `/proc/*/ns/mnt`.
pub(crate) fn lookup(id: u64) -> AxResult<Arc<Self>> {
        MOUNT_NAMESPACE_REGISTRY
            .lock()
            .get(&id)
            .and_then(Weak::upgrade)
            .ok_or(AxError::NotFound)
    }

    /// Snapshot every live mount namespace while the caller owns the mount
    /// namespace operation lock.  Propagation is a relationship between
    /// mount instances, not merely between tasks in the current namespace;
    /// keeping this lookup here prevents the mount layer from manufacturing a
    /// second namespace registry.
pub(crate) fn live() -> AxResult<Vec<Arc<Self>>> {
        let mut namespaces = MOUNT_NAMESPACE_REGISTRY.lock();
        namespaces.retain(|_, entry| entry.strong_count() != 0);
        let mut live = Vec::new();
        live.try_reserve_exact(namespaces.len())
            .map_err(|_| AxError::NoMemory)?;
        for entry in namespaces.values() {
            if let Some(namespace) = entry.upgrade() {
                live.push(namespace);
            }
        }
        Ok(live)
    }
}

impl Drop for MountNamespace {
    fn drop(&mut self) {
        crate::mounts::unregister_cloned_provider_mounts(&mut *self.provider_registrations.lock());
    }
}

impl CgroupNamespace {
pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Self::try_new(
            owner_user_ns,
            crate::pseudofs::cgroup::root_namespace_roots()?,
        )
    }

    fn try_new(
        owner_user_ns: Arc<UserNamespace>,
        roots: crate::pseudofs::cgroup::CgroupNamespaceRoots,
    ) -> AxResult<Arc<Self>> {
        let id = try_allocate_proc_namespace_id()?;
        Arc::try_new(Self {
            id,
            owner_user_ns,
            roots,
        })
        .map_err(|_| AxError::NoMemory)
    }

pub(crate) fn try_fork(
        _source: &Arc<Self>,
        owner_user_ns: Arc<UserNamespace>,
        roots: crate::pseudofs::cgroup::CgroupNamespaceRoots,
    ) -> AxResult<Arc<Self>> {
        // The new namespace root is selected from the caller's *current*
        // membership, not inherited from `self`.  A task may have migrated
        // after it entered this cgroup namespace, so copying `self.roots`
        // would incorrectly expose the old subtree to CLONE_NEWCGROUP.
        Self::try_new(owner_user_ns, roots)
    }

pub(crate) fn id(&self) -> u64 {
        self.id
    }

pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }

pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

pub(crate) fn roots(&self) -> &crate::pseudofs::cgroup::CgroupNamespaceRoots {
        &self.roots
    }
}

pub(crate) struct PidNamespace {
    id: u64,
    parent: Option<Arc<PidNamespace>>,
    /// Namespace-local PID bindings. A process owns one binding in its own
    /// namespace and every ancestor. The bindings are retained through zombie
    /// state and released only by `reap_process`.
    pub(crate) pids: SpinNoIrq<PidNamespacePids>,
    reaper_scope: Option<Arc<ProcessReaperScope>>,
    owner_user_ns: Arc<UserNamespace>,
}

/// Linux's initial PID namespace starts with the upstream `PID_MAX_DEFAULT`.
/// `pid_max` is an exclusive bound, so ordinary allocation uses
/// `1..pid_max`.
const PID_MAX_DEFAULT: Pid = 0x8000;

/// x86_64 Linux v6.18's `PID_MAX_LIMIT`.  Child PID namespaces are created
/// with this limit; the initial namespace may subsequently be constrained by
/// its `pid_max` sysctl.
pub(crate) const PID_MAX_LIMIT: Pid = 4 * 1024 * 1024;

/// Linux keeps the low PID region available during the first allocation pass
/// and restarts cyclic allocation from this point after reaching `pid_max`.
const RESERVED_PIDS: Pid = 300;
const PIDS_PER_CPU_DEFAULT: Pid = 1024;
const PIDS_PER_CPU_MIN: Pid = 8;

fn possible_cpu_count() -> Pid {
    (axhal::cpu_num().max(1).min(PID_MAX_LIMIT as usize)) as Pid
}

fn pid_max_min() -> Pid {
    (RESERVED_PIDS + 1).max(PIDS_PER_CPU_MIN.saturating_mul(possible_cpu_count()))
}

fn initial_pid_max() -> Pid {
    PID_MAX_DEFAULT
        .max(PIDS_PER_CPU_DEFAULT.saturating_mul(possible_cpu_count()))
        .min(PID_MAX_LIMIT)
}

pub(crate) struct PidNamespacePids {
    pub(crate) by_global: HashMap<Pid, Pid>,
    pub(crate) by_local: HashMap<Pid, Pid>,
    /// Exclusive local-PID ceiling for this particular namespace.
    pid_max: Pid,
    next: Pid,
    allocation_disabled: bool,
    pending_publications: usize,
}

impl PidNamespacePids {
    pub(crate) fn try_new(init_pid: Option<Pid>) -> AxResult<Self> {
        Self::try_new_with_pid_max(init_pid, PID_MAX_LIMIT)
    }

    fn try_new_with_pid_max(init_pid: Option<Pid>, pid_max: Pid) -> AxResult<Self> {
        if !(pid_max_min()..=PID_MAX_LIMIT).contains(&pid_max) {
            return Err(AxError::InvalidInput);
        }
        let mut pids = Self {
            by_global: HashMap::new(),
            by_local: HashMap::new(),
            pid_max,
            next: 1,
            allocation_disabled: false,
            pending_publications: 0,
        };
        if let Some(init_pid) = init_pid {
            pids.try_insert(init_pid, 1)?;
            pids.next = 2;
        }
        Ok(pids)
    }

    fn try_insert(&mut self, global_pid: Pid, local_pid: Pid) -> AxResult<()> {
        if !(1..self.pid_max).contains(&local_pid) {
            return Err(AxError::NoMemory);
        }
        self.by_global
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        self.by_local
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        if self.by_global.contains_key(&global_pid) || self.by_local.contains_key(&local_pid) {
            return Err(AxError::AlreadyExists);
        }
        self.by_global.insert(global_pid, local_pid);
        self.by_local.insert(local_pid, global_pid);
        Ok(())
    }

    fn try_reserve(&mut self, global_pid: Pid) -> AxResult<bool> {
        if self.by_global.contains_key(&global_pid) {
            return Ok(false);
        }
        // A `pid_max` sysctl write does not reset Linux's IDR cursor.  If
        // that cursor now lies beyond the new exclusive bound, the next
        // cyclic allocation wraps through the post-reserved PID range.
        let first = if self.next >= self.pid_max {
            RESERVED_PIDS
        } else {
            self.next.max(1)
        };
        let mut candidate = first;
        loop {
            if !self.by_local.contains_key(&candidate) {
                self.try_insert(global_pid, candidate)?;
                // Preserve the actual cyclic cursor, including the exclusive
                // bound itself.  A later `pid_max` increase must continue at
                // that former bound rather than prematurely wrap to 300.
                self.next = candidate + 1;
                return Ok(true);
            }
            candidate = if candidate + 1 == self.pid_max {
                RESERVED_PIDS
            } else {
                candidate + 1
            };
            if candidate == first {
                // Linux's PID allocator reports ID-space exhaustion as
                // EAGAIN. Keep allocation failures from `try_insert` above
                // as ENOMEM instead.
                return Err(LinuxError::EAGAIN.into());
            }
        }
    }

    pub(crate) fn try_reserve_exact(&mut self, global_pid: Pid, local_pid: Pid) -> AxResult<bool> {
        if !(1..self.pid_max).contains(&local_pid) {
            return Err(AxError::InvalidInput);
        }
        if let Some(existing) = self.by_global.get(&global_pid) {
            return (*existing == local_pid)
                .then_some(false)
                .ok_or(AxError::AlreadyExists);
        }
        self.try_insert(global_pid, local_pid)?;
        Ok(true)
    }

    fn reserve_publication(&mut self, global_pid: Pid, local_pid: Option<Pid>) -> AxResult<bool> {
        if self.allocation_disabled {
            return Err(AxError::NoMemory);
        }
        let allocated = match local_pid {
            Some(pid) => self.try_reserve_exact(global_pid, pid)?,
            None => self.try_reserve(global_pid)?,
        };
        self.pending_publications += 1;
        Ok(allocated)
    }

    fn pid_max(&self) -> Pid {
        self.pid_max
    }

    fn try_set_pid_max(&mut self, pid_max: Pid) -> AxResult<()> {
        if !(pid_max_min()..=PID_MAX_LIMIT).contains(&pid_max) {
            return Err(AxError::InvalidInput);
        }
        self.pid_max = pid_max;
        Ok(())
    }

    fn release(&mut self, global_pid: Pid) {
        let Some(local_pid) = self.by_global.remove(&global_pid) else {
            return;
        };
        let removed = self.by_local.remove(&local_pid);
        debug_assert_eq!(removed, Some(global_pid));
    }
}

/// Rollback guard for a pre-publication process PID binding. Commit leaves the
/// binding owned by the namespace until successful process reap.
pub(crate) struct PidNamespaceReservation {
    namespace: Arc<PidNamespace>,
    global_pid: Pid,
    allocated_here: bool,
    parent: Option<Box<PidNamespaceReservation>>,
    committed: bool,
}

impl PidNamespaceReservation {
pub(crate) fn commit(mut self) {
        self.commit_recursive();
    }

    fn commit_recursive(&mut self) {
        self.committed = true;
        if let Some(parent) = self.parent.as_mut() {
            parent.commit_recursive();
        }
    }
}

impl Drop for PidNamespaceReservation {
    fn drop(&mut self) {
        let mut pids = self.namespace.pids.lock();
        if !self.committed && self.allocated_here {
            pids.release(self.global_pid);
        }
        pids.pending_publications -= 1;
    }
}

impl PidNamespace {
pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Self::try_new(None, None, None, owner_user_ns)
    }

pub(crate) fn try_new_root_with_reaper_scope(
        owner_user_ns: Arc<UserNamespace>,
        reaper_scope: Arc<ProcessReaperScope>,
    ) -> AxResult<Arc<Self>> {
        Self::try_new(None, None, Some(reaper_scope), owner_user_ns)
    }

    fn try_new(
        parent: Option<Arc<Self>>,
        init_pid: Option<Pid>,
        reaper_scope: Option<Arc<ProcessReaperScope>>,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        let id = try_allocate_proc_namespace_id()?;
        let nested = parent.is_some();
        Arc::try_new(Self {
            id,
            parent,
            pids: SpinNoIrq::new(PidNamespacePids::try_new_with_pid_max(
                init_pid,
                if nested {
                    PID_MAX_LIMIT
                } else {
                    initial_pid_max()
                },
            )?),
            reaper_scope,
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

pub(crate) fn try_fork(
        self: &Arc<Self>,
        init_pid: Pid,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        Self::try_new(Some(self.clone()), Some(init_pid), None, owner_user_ns)
    }

    /// `unshare(CLONE_NEWPID)` changes only the PID namespace inherited by
    /// future children.  The calling task remains in its current namespace,
    /// so the deferred child namespace intentionally starts without PID 1.
pub(crate) fn try_fork_for_children(
        self: &Arc<Self>,
        owner_user_ns: Arc<UserNamespace>,
        reaper_scope: Arc<ProcessReaperScope>,
    ) -> AxResult<Arc<Self>> {
        Self::try_new(Some(self.clone()), None, Some(reaper_scope), owner_user_ns)
    }

pub(crate) fn try_fork_with_reaper_scope(
        self: &Arc<Self>,
        init_pid: Pid,
        owner_user_ns: Arc<UserNamespace>,
        reaper_scope: Arc<ProcessReaperScope>,
    ) -> AxResult<Arc<Self>> {
        Self::try_new(
            Some(self.clone()),
            Some(init_pid),
            Some(reaper_scope),
            owner_user_ns,
        )
    }

pub(crate) fn parent(&self) -> Option<Arc<Self>> {
        self.parent.clone()
    }

pub(crate) fn reaper_scope(&self) -> Option<Arc<ProcessReaperScope>> {
        self.reaper_scope.clone()
    }

pub(crate) fn has_no_init(&self) -> bool {
        self.pids.lock().by_global.is_empty()
    }

    /// Close allocation under the same lock as reservation, then let every
    /// already admitted clone publish or roll back before the exit walk.
pub(crate) fn disable_allocation(&self) {
        self.pids.lock().allocation_disabled = true;
    }

pub(crate) fn has_pending_publications(&self) -> bool {
        self.pids.lock().pending_publications != 0
    }

    /// Linux disables PID allocation when a namespace's child reaper exits.
    /// The process core retains whether its scope init was ever bound, so a
    /// reaped init cannot make a dead namespace look newly created. The latter
    /// deliberately reports ENOMEM, matching alloc_pid()'s long-standing
    /// externally visible result rather than leaking a core NotLive detail.
    pub(crate) fn child_reaper_allows_new_processes(&self) -> bool {
        match self.reaper_scope() {
            None => true,
            Some(scope) => match scope.init_process() {
                // A CLONE_NEWPID/unshare first child has reserved a namespace
                // but has not yet atomically published its scope init.
                None => !scope.was_initialized(),
                Some(init) => init.is_live(),
            },
        }
    }

    /// Last PID allocated in this namespace, including subsequently reaped tasks.
pub(crate) fn last_allocated_pid(&self) -> Pid {
        self.pids.lock().next.saturating_sub(1)
    }

    /// The namespace-local, exclusive PID allocation bound.
pub(crate) fn pid_max(&self) -> Pid {
        self.pids.lock().pid_max()
    }

    /// Applies a validated namespace-local `pid_max` sysctl value. Existing
    /// bindings remain valid when the ceiling is lowered, as on Linux; only
    /// future automatic or explicit allocations are constrained by it.
pub(crate) fn try_set_pid_max(&self, pid_max: Pid) -> AxResult<()> {
        self.pids.lock().try_set_pid_max(pid_max)
    }

    /// Reserves one local PID in this namespace and all of its ancestors.
    /// The returned guard must be committed only once the child has reached
    /// process publication; otherwise it restores every newly allocated slot.
pub(crate) fn reserve_process(
        self: &Arc<Self>,
        global_pid: Pid,
    ) -> AxResult<PidNamespaceReservation> {
        if !self.child_reaper_allows_new_processes() {
            return Err(AxError::NoMemory);
        }
        let parent = self
            .parent()
            .map(|parent| parent.reserve_process(global_pid))
            .transpose()?
            .map(Box::new);
        let allocated_here = self.pids.lock().reserve_publication(global_pid, None)?;
        Ok(PidNamespaceReservation {
            namespace: self.clone(),
            global_pid,
            allocated_here,
            parent,
            committed: false,
        })
    }

    /// Reserves a clone3 `set_tid` vector from this namespace out through its
    /// ancestors. The vector is ordered innermost-to-outermost, and the guard
    /// releases every acquired slot if any later namespace rejects it.
pub(crate) fn reserve_process_with_ids(
        self: &Arc<Self>,
        global_pid: Pid,
        requested: &[Pid],
        actor: &Cred,
    ) -> AxResult<PidNamespaceReservation> {
        if !self.child_reaper_allows_new_processes() {
            return Err(AxError::NoMemory);
        }
        fn depth(namespace: &PidNamespace) -> usize {
            namespace.parent().map_or(1, |parent| depth(&parent) + 1)
        }
        if requested.len() > depth(self) {
            return Err(AxError::InvalidInput);
        }

        fn reserve(
            namespace: &Arc<PidNamespace>,
            global_pid: Pid,
            requested: &[Pid],
            actor: &Cred,
            level: usize,
        ) -> AxResult<PidNamespaceReservation> {
            // Explicit clone3 IDs must not bypass a dead ancestor's reaper.
            if !namespace.child_reaper_allows_new_processes() {
                return Err(AxError::NoMemory);
            }
            let allocated_here = if let Some(&local_pid) = requested.get(level) {
                {
                    let pids = namespace.pids.lock();
                    if !(1..pids.pid_max()).contains(&local_pid) {
                        return Err(AxError::InvalidInput);
                    }
                    // A namespace can only receive a non-init PID after its
                    // PID 1 exists. Check this before privilege so malformed
                    // namespace state does not become an authorization oracle.
                    if local_pid != 1 && !pids.by_local.contains_key(&1) {
                        return Err(AxError::InvalidInput);
                    }
                }
                if !crate::task::ns_capable(
                    actor,
                    namespace.owner_user_ns(),
                    linux_raw_sys::general::CAP_CHECKPOINT_RESTORE,
                ) && !crate::task::ns_capable(
                    actor,
                    namespace.owner_user_ns(),
                    linux_raw_sys::general::CAP_SYS_ADMIN,
                ) {
                    return Err(AxError::OperationNotPermitted);
                }
                namespace
                    .pids
                    .lock()
                    .reserve_publication(global_pid, Some(local_pid))?
            } else {
                namespace
                    .pids
                    .lock()
                    .reserve_publication(global_pid, None)?
            };
            let mut reservation = PidNamespaceReservation {
                namespace: namespace.clone(),
                global_pid,
                allocated_here,
                parent: None,
                committed: false,
            };
            if let Some(parent) = namespace.parent() {
                reservation.parent = Some(Box::new(reserve(
                    &parent,
                    global_pid,
                    requested,
                    actor,
                    level + 1,
                )?));
            }
            Ok(reservation)
        }

        reserve(self, global_pid, requested, actor, 0)
    }

    /// Releases the namespace PID binding after its final identity owner has
    /// gone. This is normally authoritative reap; `setsid` also uses it for
    /// an already reaped session leader when the final group leaves its
    /// session. Exit intentionally does not call this: zombies retain numeric
    /// identity until wait/autoreap consumes them.
pub(crate) fn release_reaped_process(&self, global_pid: Pid) {
        self.pids.lock().release(global_pid);
        if let Some(parent) = self.parent() {
            parent.release_reaped_process(global_pid);
        }
    }

    /// Releases a non-leader thread ID after its core membership has been
    /// unlinked. Process IDs deliberately use `release_reaped_process` so a
    /// zombie remains numerically addressable until wait/autoreap.
pub(crate) fn release_exited_thread(&self, global_tid: Pid) {
        self.pids.lock().release(global_tid);
        if let Some(parent) = self.parent() {
            parent.release_exited_thread(global_tid);
        }
    }

    /// Returns whether `target` is this namespace or one of its descendants.
    /// A caller can address tasks in descendants, but never tasks in an
    /// unrelated or ancestor PID namespace.
pub(crate) fn contains(&self, target: &Arc<Self>) -> bool {
        let mut candidate = Some(target.clone());
        while let Some(namespace) = candidate {
            if core::ptr::eq(self, &*namespace) {
                return true;
            }
            candidate = namespace.parent();
        }
        false
    }

    /// Renders a global process/thread identifier in this caller namespace,
    /// returning `None` when the target namespace is not visible here.
pub(crate) fn visible_pid_for(
        &self,
        target_namespace: &Arc<Self>,
        global_pid: Pid,
    ) -> Option<Pid> {
        self.contains(target_namespace)
            .then(|| self.visible_pid_checked(global_pid))
            .flatten()
    }

pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

pub(crate) fn visible_pid(&self, global_pid: Pid) -> Pid {
        // Kernel-created processes always hold a binding until reap. Retain a
        // global fallback for synthetic/unit-test identities that predate the
        // allocator and have not been admitted through the kernel lifecycle.
        self.pids
            .lock()
            .by_global
            .get(&global_pid)
            .copied()
            .unwrap_or(global_pid)
    }

    /// Strict syscall-visible rendering: absence of a namespace binding is
    /// not a global PID and must be rendered as zero by pid_vnr-style users.
pub(crate) fn visible_pid_checked(&self, global_pid: Pid) -> Option<Pid> {
        self.pids.lock().by_global.get(&global_pid).copied()
    }

    /// Resolves a positive PID visible in this namespace to its kernel-wide
    /// identity. Unlike [`Self::visible_pid`], this never invents a fallback:
    /// syscall lookup must not turn an unseen namespace-local number into a
    /// global task lookup.
pub(crate) fn resolve_visible_pid(&self, visible_pid: Pid) -> Option<Pid> {
        (visible_pid != 0)
            .then(|| self.pids.lock().by_local.get(&visible_pid).copied())
            .flatten()
    }

pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }
}

pub(crate) struct UserNamespace {
    _admission: UserNamespaceAdmission,
    id: u64,
    domain: UserNamespaceDomain<UserNamespace>,
    pub(crate) map_state: SpinNoIrq<UserNamespaceMapState>,
    signal_accounts: SignalAccountRegistryMutex<HashMap<Kuid, Weak<SignalQueueAccount>>>,
    global_signal_account: Arc<SignalQueueAccount>,
}

impl UserNamespace {
pub(crate) fn try_new_root() -> AxResult<Arc<Self>> {
        let map_state = UserNamespaceMapState::try_initial().map_err(cred_error)?;
        let global_signal_account = SignalQueueAccount::try_new(SIGNAL_QUEUE_GLOBAL_HARD_LIMIT)
            .map_err(|_| AxError::NoMemory)?;
        let admission = UserNamespaceAdmission::try_new()?;
        let id = try_allocate_proc_namespace_id()?;
        Arc::try_new(Self {
            _admission: admission,
            id,
            domain: UserNamespaceDomain::initial(),
            map_state: SpinNoIrq::new(map_state),
            signal_accounts: SignalAccountRegistryMutex::new(HashMap::new()),
            global_signal_account,
        })
        .map_err(|_| AxError::NoMemory)
    }

pub(crate) fn try_fork(
        self: &Arc<Self>,
        owner: Kuid,
        group: Kgid,
        parent_could_setfcap: bool,
    ) -> AxResult<Arc<Self>> {
        let (uid_map, gid_map, setgroups_allowed) = {
            let state = self.map_state.lock();
            (state.uid_map(), state.gid_map(), state.setgroups_allowed())
        };
        let domain = UserNamespaceDomain::try_child(
            self,
            &uid_map,
            &gid_map,
            owner,
            group,
            parent_could_setfcap,
        )
        .map_err(cred_error)?;
        let map_state = UserNamespaceMapState::try_child(setgroups_allowed).map_err(cred_error)?;
        let admission = UserNamespaceAdmission::try_new()?;
        let id = try_allocate_proc_namespace_id()?;
        Arc::try_new(Self {
            _admission: admission,
            id,
            domain,
            map_state: SpinNoIrq::new(map_state),
            signal_accounts: SignalAccountRegistryMutex::new(HashMap::new()),
            global_signal_account: self.global_signal_account.clone(),
        })
        .map_err(|_| AxError::NoMemory)
    }

pub(crate) fn parent(&self) -> Option<Arc<Self>> {
        self.domain.parent()
    }

    /// Returns the stable, non-owning identity of this namespace.
pub(crate) const fn identity(&self) -> UserNamespaceId {
        UserNamespaceId(self.id)
    }

pub(crate) fn is_initial(&self) -> bool {
        self.domain.is_initial()
    }

pub(crate) fn owner_kuid(&self) -> Kuid {
        self.domain.owner_kuid()
    }

pub(crate) fn parent_could_setfcap(&self) -> bool {
        self.domain.parent_could_setfcap()
    }

pub(crate) fn uid_map(&self) -> Arc<IdMap> {
        self.map_state.lock().uid_map()
    }

pub(crate) fn gid_map(&self) -> Arc<IdMap> {
        self.map_state.lock().gid_map()
    }

    fn map_display_namespace(self: &Arc<Self>, viewer: &Arc<Self>) -> Arc<Self> {
        // Linux uses seq_user_ns() for map reads, except that a task reading
        // its own namespace map sees lower IDs in the immediate parent.
        if Arc::ptr_eq(self, viewer) {
            self.parent().unwrap_or_else(|| viewer.clone())
        } else {
            viewer.clone()
        }
    }

pub(crate) fn try_uid_map_rows(
        self: &Arc<Self>,
        viewer: &Arc<Self>,
    ) -> AxResult<Vec<IdMapInputExtent>> {
        let map = self.uid_map();
        let lower_map = self.map_display_namespace(viewer).uid_map();
        map.try_extents_for_lower(&lower_map).map_err(cred_error)
    }

pub(crate) fn try_gid_map_rows(
        self: &Arc<Self>,
        viewer: &Arc<Self>,
    ) -> AxResult<Vec<IdMapInputExtent>> {
        let map = self.gid_map();
        let lower_map = self.map_display_namespace(viewer).gid_map();
        map.try_extents_for_lower(&lower_map).map_err(cred_error)
    }

pub(crate) fn try_build_uid_map(&self, input: Vec<IdMapInputExtent>) -> AxResult<Arc<IdMap>> {
        let parent = self.parent().ok_or(AxError::OperationNotPermitted)?;
        let parent_map = parent.uid_map();
        IdMap::try_from_parent(input, &parent_map).map_err(cred_error)
    }

pub(crate) fn try_build_uid_map_from_slice(
        &self,
        input: &[IdMapInputExtent],
    ) -> AxResult<Arc<IdMap>> {
        let parent = self.parent().ok_or(AxError::OperationNotPermitted)?;
        let parent_map = parent.uid_map();
        IdMap::try_from_parent_slice(input, &parent_map).map_err(cred_error)
    }

pub(crate) fn try_build_gid_map(&self, input: Vec<IdMapInputExtent>) -> AxResult<Arc<IdMap>> {
        let parent = self.parent().ok_or(AxError::OperationNotPermitted)?;
        let parent_map = parent.gid_map();
        IdMap::try_from_parent(input, &parent_map).map_err(cred_error)
    }

pub(crate) fn try_build_gid_map_from_slice(
        &self,
        input: &[IdMapInputExtent],
    ) -> AxResult<Arc<IdMap>> {
        let parent = self.parent().ok_or(AxError::OperationNotPermitted)?;
        let parent_map = parent.gid_map();
        IdMap::try_from_parent_slice(input, &parent_map).map_err(cred_error)
    }

    /// Publishes a fully built UID map exactly once. Construction and parent
    /// resolution happen before the short map-state guard. The core borrows
    /// `map` and clones it into an empty slot, so no map ownership is retired or
    /// returned by the guarded operation.
pub(crate) fn publish_uid_map(&self, map: Arc<IdMap>) -> AxResult<()> {
        let result = {
            let mut state = self.map_state.lock();
            state.try_publish_uid_map(&map)
        };
        result.map_err(cred_error)
    }

    /// Publishes a fully built GID map exactly once. Unprivileged callers pass
    /// `require_setgroups_denied`; that check is made under the same map-state
    /// guard as publication, closing the deny/write race.
pub(crate) fn publish_gid_map(
        &self,
        map: Arc<IdMap>,
        require_setgroups_denied: bool,
    ) -> AxResult<()> {
        let result = {
            let mut state = self.map_state.lock();
            state.try_publish_gid_map(&map, require_setgroups_denied)
        };
        result.map_err(cred_error)
    }

pub(crate) fn setgroups_allowed(&self) -> bool {
        self.map_state.lock().setgroups_allowed()
    }

pub(crate) fn uid_map_written(&self) -> bool {
        self.map_state.lock().uid_map_written()
    }

pub(crate) fn gid_map_written(&self) -> bool {
        self.map_state.lock().gid_map_written()
    }

pub(crate) fn may_setgroups(&self) -> bool {
        self.map_state.lock().may_setgroups()
    }

pub(crate) fn update_setgroups_policy(&self, allow: bool) -> AxResult<()> {
        self.map_state
            .lock()
            .try_update_setgroups_policy(allow)
            .map_err(cred_error)
    }

pub(crate) fn user_uid_to_kernel(&self, uid: UserUid) -> Option<Kuid> {
        self.uid_map().user_uid_to_kernel(uid)
    }

pub(crate) fn kernel_uid_to_user(&self, uid: Kuid) -> Option<UserUid> {
        self.uid_map().kernel_uid_to_user(uid)
    }

pub(crate) fn user_gid_to_kernel(&self, gid: UserGid) -> Option<Kgid> {
        self.gid_map().user_gid_to_kernel(gid)
    }

pub(crate) fn kernel_gid_to_user(&self, gid: Kgid) -> Option<UserGid> {
        self.gid_map().kernel_gid_to_user(gid)
    }

pub(crate) fn make_kuid(&self, uid: u32) -> Option<Kuid> {
        UserUid::from_raw(uid).and_then(|uid| self.user_uid_to_kernel(uid))
    }

pub(crate) fn root_kuid(&self) -> Option<Kuid> {
        self.user_uid_to_kernel(UserUid::ROOT)
    }

pub(crate) fn make_kgid(&self, gid: u32) -> Option<Kgid> {
        UserGid::from_raw(gid).and_then(|gid| self.user_gid_to_kernel(gid))
    }

pub(crate) fn root_kgid(&self) -> Option<Kgid> {
        self.user_gid_to_kernel(UserGid::ROOT)
    }

    // Named after Linux's `from_kuid_munged(struct user_namespace *, kuid_t)`,
    // where the namespace is the subject and the kuid is the operand. Renaming
    // to satisfy the `from_*` convention would break that correspondence.
    #[allow(clippy::wrong_self_convention)]
pub(crate) fn from_kuid_munged(&self, uid: Kuid) -> u32 {
        self.kernel_uid_to_user(uid)
            .map(UserUid::into_raw)
            .unwrap_or(USER_NAMESPACE_OVERFLOW_ID)
    }

    #[allow(clippy::wrong_self_convention)]
pub(crate) fn from_kgid_munged(&self, gid: Kgid) -> u32 {
        self.kernel_gid_to_user(gid)
            .map(UserGid::into_raw)
            .unwrap_or(USER_NAMESPACE_OVERFLOW_ID)
    }

    /// Returns the RT signal queue accounts for a real UID in this namespace.
    ///
    /// Registry allocation is fallible and happens under a sleepable mutex,
    /// never under a signal pending SpinNoIrq guard. A losing candidate is
    /// dropped only after the registry guard has been released.
pub(crate) fn try_signal_queue_accounts(
        &self,
        real_uid: Kuid,
    ) -> AxResult<(Arc<SignalQueueAccount>, Arc<SignalQueueAccount>)> {
        let existing = {
            let accounts = self.signal_accounts.lock();
            accounts.get(&real_uid).and_then(Weak::upgrade)
        };
        if let Some(existing) = existing {
            return Ok((existing, self.global_signal_account.clone()));
        }

        let candidate = SignalQueueAccount::try_new(SIGNAL_QUEUE_PER_USER_HARD_LIMIT)
            .map_err(|_| AxError::NoMemory)?;
        let winner = {
            let mut accounts = self.signal_accounts.lock();
            if let Some(existing) = accounts.get(&real_uid).and_then(Weak::upgrade) {
                Some(existing)
            } else {
                accounts.retain(|_, account| account.strong_count() != 0);
                accounts.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                accounts.insert(real_uid, Arc::downgrade(&candidate));
                None
            }
        };

        if let Some(winner) = winner {
            drop(candidate);
            Ok((winner, self.global_signal_account.clone()))
        } else {
            Ok((candidate, self.global_signal_account.clone()))
        }
    }

pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }
}

impl tk_linux_cred::UserNamespaceView for UserNamespace {
    fn parent(self: &Arc<Self>) -> Option<Arc<Self>> {
        self.domain.parent()
    }

    fn level(&self) -> u32 {
        self.domain.level()
    }

    fn owner_kuid(&self) -> Kuid {
        self.domain.owner_kuid()
    }

    fn root_kuid(&self) -> Option<Kuid> {
        UserNamespace::root_kuid(self)
    }

    fn is_initial(&self) -> bool {
        self.domain.is_initial()
    }
}

impl tk_linux_cred::ExecUserNamespaceView for UserNamespace {
    fn exec_id_map_snapshot(&self) -> (Arc<IdMap>, Arc<IdMap>) {
        let state = self.map_state.lock();
        (state.uid_map(), state.gid_map())
    }
}

#[derive(Clone, Copy)]
pub(crate) struct UtsState {
    pub(crate) nodename: [u8; UTS_FIELD_LEN],
    pub(crate) nodename_len: usize,
    pub(crate) domainname: [u8; UTS_FIELD_LEN],
    pub(crate) domainname_len: usize,
}

const fn copy_uts_field(dst: &mut [u8; UTS_FIELD_LEN], src: &[u8]) -> usize {
    let len = if src.len() < UTS_FIELD_LEN {
        src.len()
    } else {
        UTS_FIELD_LEN
    };
    let mut index = 0;
    while index < len {
        dst[index] = src[index];
        index += 1;
    }
    len
}

pub(crate) const fn init_uts_state() -> UtsState {
    let mut state = UtsState {
        nodename: [0; UTS_FIELD_LEN],
        nodename_len: 0,
        domainname: [0; UTS_FIELD_LEN],
        domainname_len: 0,
    };
    state.nodename_len = copy_uts_field(&mut state.nodename, b"thekernel");
    state.domainname_len = copy_uts_field(&mut state.domainname, b"(none)");
    state
}

impl UtsState {
    fn set_nodename(&mut self, value: &[u8]) {
        self.nodename = [0; UTS_FIELD_LEN];
        self.nodename_len = copy_uts_field(&mut self.nodename, value);
    }

    fn set_domainname(&mut self, value: &[u8]) {
        self.domainname = [0; UTS_FIELD_LEN];
        self.domainname_len = copy_uts_field(&mut self.domainname, value);
    }
}

pub(crate) struct UtsNamespace {
    id: u64,
    state: SpinNoIrq<UtsState>,
    owner_user_ns: Arc<UserNamespace>,
}

impl UtsNamespace {
pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            id: try_allocate_proc_namespace_id()?,
            state: SpinNoIrq::new(init_uts_state()),
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

pub(crate) fn try_fork(&self, owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        let state = *self.state.lock();
        Arc::try_new(Self {
            id: try_allocate_proc_namespace_id()?,
            state: SpinNoIrq::new(state),
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

pub(crate) const fn id(&self) -> u64 {
        self.id
    }
pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }

pub(crate) fn nodename(&self) -> AxResult<Vec<u8>> {
        let state = *self.state.lock();
        Ok(state.nodename[..state.nodename_len].to_vec())
    }

pub(crate) fn domainname(&self) -> AxResult<Vec<u8>> {
        let state = *self.state.lock();
        Ok(state.domainname[..state.domainname_len].to_vec())
    }

    /// Snapshot both UTS name fields under one lock acquisition.
    ///
    /// `uname(2)` observes the namespace state as one unit, so its nodename
    /// and domainname must not come from separate writer generations.
pub(crate) fn names_snapshot(&self) -> ([u8; UTS_FIELD_LEN], [u8; UTS_FIELD_LEN]) {
        let state = *self.state.lock();
        (state.nodename, state.domainname)
    }

pub(crate) fn set_nodename(&self, value: &[u8]) -> AxResult<()> {
        self.state.lock().set_nodename(value);
        Ok(())
    }

pub(crate) fn set_domainname(&self, value: &[u8]) -> AxResult<()> {
        self.state.lock().set_domainname(value);
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
struct TimeNamespaceState {
    monotonic_offset_ns: i64,
    boottime_offset_ns: i64,
}

pub(crate) struct TimeNamespace {
    id: u64,
    state: SpinNoIrq<TimeNamespaceState>,
    owner_user_ns: Arc<UserNamespace>,
}

impl TimeNamespace {
pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            id: try_allocate_proc_namespace_id()?,
            state: SpinNoIrq::new(TimeNamespaceState::default()),
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

pub(crate) fn try_fork(&self, owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        let state = *self.state.lock();
        Arc::try_new(Self {
            id: try_allocate_proc_namespace_id()?,
            state: SpinNoIrq::new(state),
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

pub(crate) const fn id(&self) -> u64 {
        self.id
    }
pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }

    fn offset_ns(&self, boottime: bool) -> i64 {
        let state = self.state.lock();
        if boottime {
            state.boottime_offset_ns
        } else {
            state.monotonic_offset_ns
        }
    }

pub(crate) fn apply_monotonic_offset(&self, value: Duration) -> Duration {
        apply_time_offset(value, self.offset_ns(false))
    }

pub(crate) fn apply_boottime_offset(&self, value: Duration) -> Duration {
        apply_time_offset(value, self.offset_ns(true))
    }

pub(crate) fn host_monotonic_deadline(&self, value: Duration) -> Duration {
        apply_time_offset(value, self.offset_ns(false).saturating_neg())
    }

pub(crate) fn host_boottime_deadline(&self, value: Duration) -> Duration {
        apply_time_offset(value, self.offset_ns(true).saturating_neg())
    }

pub(crate) fn set_monotonic_offset(&self, secs: i64, nsecs: u32) {
        self.state.lock().monotonic_offset_ns = offset_to_nanos(secs, nsecs);
    }

pub(crate) fn set_boottime_offset(&self, secs: i64, nsecs: u32) {
        self.state.lock().boottime_offset_ns = offset_to_nanos(secs, nsecs);
    }

pub(crate) fn render_offsets(&self) -> Vec<u8> {
        let state = *self.state.lock();
        let (mono_sec, mono_nsec) = nanos_to_offset(state.monotonic_offset_ns);
        let (boot_sec, boot_nsec) = nanos_to_offset(state.boottime_offset_ns);
        format!("monotonic  {mono_sec:10} {mono_nsec:9}\nboottime   {boot_sec:10} {boot_nsec:9}\n")
            .into_bytes()
    }
}

/// Linux-visible network namespace identity over one generic network stack.
///
/// `NetStack` stays in the generic mechanism layer. The owning user namespace
/// belongs to this Linux-ABI object so authority remains bound to the object
/// even after the creating process exits or a socket crosses processes.
pub(crate) struct NetworkNamespace {
    id: u64,
    stack: Arc<NetStack>,
    owner_user_ns: Arc<UserNamespace>,
}

impl NetworkNamespace {
pub(crate) fn try_new(
        stack: Arc<NetStack>,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        let namespace = Arc::try_new(Self {
            id: try_allocate_proc_namespace_id()?,
            stack,
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)?;
        // The pre-protocol seam is the XDP ingress point: a socket binding is
        // never consulted here.  Only a program attached to this exact
        // namespace/interface can consume the frame through a typed XSKMAP
        // redirect target.
        let weak = Arc::downgrade(&namespace);
        namespace
            .stack
            .set_pre_protocol_hook(Some(Arc::new(move |ifindex, packet| {
                let namespace = weak.upgrade().ok_or(AxError::BadState)?;
                let Some(program) = crate::bpf::xdp_program_snapshot(&namespace, ifindex) else {
                    return Ok(PacketAction::Pass);
                };
                let terminal = program.run_xdp(
                    crate::bpf::helpers::XdpContext {
                        data: 0,
                        data_end: packet.len().try_into().unwrap_or(u32::MAX),
                        data_meta: 0,
                        ingress_ifindex: ifindex,
                        rx_queue_index: 0,
                        egress_ifindex: 0,
                    },
                    packet,
                )?;
                match terminal {
                    crate::bpf::helpers::XdpExecutionResult::Pass => Ok(PacketAction::Pass),
                    crate::bpf::helpers::XdpExecutionResult::Redirect(redirect) => {
                        // The endpoint owns a retained UMEM capability; router
                        // bytes are copied only after the program selected this
                        // exact XSKMAP slot.  A full/invalid target is a failed
                        // redirect, never a fallback to normal protocol input.
                        if redirect.target.accepts_xdp_redirect(&namespace, ifindex)
                            && redirect.target.redirect_packet(packet, 0).unwrap_or(false)
                        {
                            Ok(PacketAction::RedirectConsumed)
                        } else {
                            match redirect.flags & 0x3 {
                                2 => Ok(PacketAction::Pass),
                                3 => Ok(PacketAction::Tx),
                                _ => Ok(PacketAction::Drop),
                            }
                        }
                    }
                    crate::bpf::helpers::XdpExecutionResult::Tx => Ok(PacketAction::Tx),
                    crate::bpf::helpers::XdpExecutionResult::Aborted
                    | crate::bpf::helpers::XdpExecutionResult::Drop
                    | crate::bpf::helpers::XdpExecutionResult::RedirectMiss
                    | crate::bpf::helpers::XdpExecutionResult::Invalid(_) => Ok(PacketAction::Drop),
                }
            })));
        let weak = Arc::downgrade(&namespace);
        namespace
            .stack
            .set_packet_hook(Some(Arc::new(move |context: &PacketContext, packet| {
                let namespace = weak.upgrade().ok_or(AxError::BadState)?;
                let hook = match context.point {
                    PacketHookPoint::Prerouting => crate::file::netlink::NftHook::Prerouting,
                    PacketHookPoint::Input => crate::file::netlink::NftHook::Input,
                    PacketHookPoint::Forward => crate::file::netlink::NftHook::Forward,
                    PacketHookPoint::LocalOutput => crate::file::netlink::NftHook::Output,
                    PacketHookPoint::Postrouting => crate::file::netlink::NftHook::Postrouting,
                };
                let ipt_hook = match context.point {
                    PacketHookPoint::Prerouting => 0,
                    PacketHookPoint::Input => 1,
                    PacketHookPoint::Forward => 2,
                    PacketHookPoint::LocalOutput => 3,
                    PacketHookPoint::Postrouting => 4,
                };
                crate::syscall::iptables_hook_verdict(&namespace, ipt_hook)?;
                // `nft_packet_hook` owns the ordered NF/BPF traversal.  Keeping
                // BPF dispatch there prevents a namespace hook from executing a
                // mutating program twice at one policy seam.
                crate::file::netlink::nft_packet_hook(&namespace, hook, packet)?;
                Ok(PacketAction::Pass)
            })));
        #[cfg(feature = "bpf")]
        {
            let weak = Arc::downgrade(&namespace);
            namespace
                .stack
                .set_packet_defrag_query(Some(Arc::new(move |point, packet| {
                    let Some(namespace) = weak.upgrade() else {
                        return false;
                    };
                    let hook = match point {
                        // Linux's BPF netfilter IP_DEFRAG is meaningful before an
                        // ingress NF hook; other seams already receive a complete
                        // local/forwarded packet in this stack.
                        PacketHookPoint::Prerouting => crate::file::bpf::BpfNetworkHook::Prerouting,
                        PacketHookPoint::Input => crate::file::bpf::BpfNetworkHook::Input,
                        PacketHookPoint::Forward => crate::file::bpf::BpfNetworkHook::Forward,
                        PacketHookPoint::LocalOutput => crate::file::bpf::BpfNetworkHook::Output,
                        PacketHookPoint::Postrouting => {
                            crate::file::bpf::BpfNetworkHook::Postrouting
                        }
                    };
                    crate::bpf::network_packet_defrag_required(&namespace, hook, packet)
                })));
        }
        Ok(namespace)
    }

pub(crate) fn try_new_loopback_only(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Self::try_new(NetStack::try_new_loopback_only()?, owner_user_ns)
    }

pub(crate) fn try_new_network_namespace(
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        Self::try_new(NetStack::try_new_network_namespace()?, owner_user_ns)
    }

pub(crate) fn stack(&self) -> &Arc<NetStack> {
        &self.stack
    }

pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

pub(crate) const fn id(&self) -> u64 {
        self.id
    }
pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }
}

/// The complete Linux namespace attachment of one task.
///
/// Namespace-changing syscalls construct a complete replacement first and
/// exchange it only at commit, so observers cannot see (for example) a new
/// user namespace paired with an old IPC or mount namespace.
#[derive(Clone)]
pub(crate) struct NamespaceProxy {
    user: Arc<UserNamespace>,
    pid: Arc<PidNamespace>,
    pid_for_children: Arc<PidNamespace>,
    mount: Arc<MountNamespace>,
    ipc: Arc<IpcNamespace>,
    net: Arc<NetworkNamespace>,
    cgroup: Arc<CgroupNamespace>,
    uts: Arc<UtsNamespace>,
    time: Arc<TimeNamespace>,
    time_for_children: Arc<TimeNamespace>,
}

impl NamespaceProxy {
    #[allow(clippy::too_many_arguments)]
pub(crate) fn try_new(
        user: Arc<UserNamespace>,
        pid: Arc<PidNamespace>,
        mount: Arc<MountNamespace>,
        ipc: Arc<IpcNamespace>,
        net: Arc<NetworkNamespace>,
        cgroup: Arc<CgroupNamespace>,
        uts: Arc<UtsNamespace>,
        time: Arc<TimeNamespace>,
    ) -> AxResult<Self> {
        // Namespace objects retain their own owner.  Mixed-owner bundles are
        // valid: CLONE_NEWUSER changes the caller's credential namespace
        // while inherited mount/net/ipc objects remain owned by the namespace
        // which created them.
        Ok(Self {
            user,
            pid: pid.clone(),
            pid_for_children: pid,
            mount,
            ipc,
            net,
            cgroup,
            uts,
            time: time.clone(),
            time_for_children: time,
        })
    }

pub(crate) fn user(&self) -> Arc<UserNamespace> {
        self.user.clone()
    }
pub(crate) fn pid(&self) -> Arc<PidNamespace> {
        self.pid.clone()
    }
pub(crate) fn pid_for_children(&self) -> Arc<PidNamespace> {
        self.pid_for_children.clone()
    }
pub(crate) fn mount(&self) -> Arc<MountNamespace> {
        self.mount.clone()
    }
pub(crate) fn ipc(&self) -> Arc<IpcNamespace> {
        self.ipc.clone()
    }
pub(crate) fn net(&self) -> Arc<NetworkNamespace> {
        self.net.clone()
    }
pub(crate) fn cgroup(&self) -> Arc<CgroupNamespace> {
        self.cgroup.clone()
    }
pub(crate) fn uts(&self) -> Arc<UtsNamespace> {
        self.uts.clone()
    }
pub(crate) fn time(&self) -> Arc<TimeNamespace> {
        self.time.clone()
    }
pub(crate) fn time_for_children(&self) -> Arc<TimeNamespace> {
        self.time_for_children.clone()
    }

pub(crate) fn replace_uts(&mut self, value: Arc<UtsNamespace>) {
        self.uts = value;
    }
pub(crate) fn replace_user(&mut self, value: Arc<UserNamespace>) {
        self.user = value;
    }
pub(crate) fn replace_pid(&mut self, value: Arc<PidNamespace>) {
        self.pid = value;
    }
pub(crate) fn replace_pid_for_children(&mut self, value: Arc<PidNamespace>) {
        self.pid_for_children = value;
    }
pub(crate) fn replace_time(&mut self, value: Arc<TimeNamespace>) {
        self.time = value.clone();
        self.time_for_children = value;
    }
pub(crate) fn replace_time_for_children(&mut self, value: Arc<TimeNamespace>) {
        self.time_for_children = value;
    }
pub(crate) fn replace_mount(&mut self, value: Arc<MountNamespace>) {
        self.mount = value;
    }
pub(crate) fn replace_ipc(&mut self, value: Arc<IpcNamespace>) {
        self.ipc = value;
    }
pub(crate) fn replace_net(&mut self, value: Arc<NetworkNamespace>) {
        self.net = value;
    }
pub(crate) fn replace_cgroup(&mut self, value: Arc<CgroupNamespace>) {
        self.cgroup = value;
    }
}

/// Prepared namespace aggregate exchange. Every allocation and authority
/// check happens before this token is created; it is committed into the
/// calling task's namespace slot.
pub(crate) struct PreparedNamespaceProxyReplacement {
pub(crate)     replacement: NamespaceProxy,
}

/// Task-owned Linux `sem_undo` list. `CLONE_SYSVSEM` shares this Arc across
/// task owners; ordinary clone snapshots it. The final owner applies its
/// adjustments to the IPC namespace that owns the semaphore arrays.
pub(crate) struct SemUndoState {
    ipc_ns: Arc<IpcNamespace>,
    undo: Mutex<Option<SemUndo>>,
}

impl SemUndoState {
pub(crate) fn try_new(ipc_ns: Arc<IpcNamespace>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            ipc_ns,
            undo: Mutex::new(Some(SemUndo::new())),
        })
        .map_err(|_| AxError::NoMemory)
    }

pub(crate) fn undo(&self) -> &Mutex<Option<SemUndo>> {
        &self.undo
    }

    /// `pid` is the detaching owner's `task_tgid()`, which Linux republishes as
    /// each adjusted semaphore's `sempid` (`ipc/sem.c:2430-2438`).
pub(crate) fn apply_on_final_exit(&self, pid: Pid) {
        let Some(mut undo) = self.undo.lock().take() else {
            return;
        };
        apply_sem_undo(self.ipc_ns.sem_manager(), &mut undo, pid);
    }
}

impl PreparedNamespaceProxyReplacement {
pub(crate) fn commit(self, thread: &crate::task::thread::Thread) {
        let old = {
            let _publication = crate::task::fs_context_publication();
            self.commit_under_publication(thread)
        };
        drop(old);
    }

    /// Exchanges only the namespace pointer while the caller already owns the
    /// publication gate.  Resource retirement is intentionally returned to
    /// the caller: dropping a proxy can cascade into VFS/IPC teardown and
    /// must never occur inside the IRQ/preemption-off publication region.
pub(crate) fn commit_under_publication(
        self,
        thread: &crate::task::thread::Thread,
    ) -> NamespaceProxy {
        core::mem::replace(&mut *thread.namespaces.lock(), self.replacement)
    }
}

fn offset_to_nanos(secs: i64, nsecs: u32) -> i64 {
    secs.saturating_mul(1_000_000_000)
        .saturating_add(nsecs as i64)
}

fn nanos_to_offset(nanos: i64) -> (i64, u32) {
    let secs = nanos.div_euclid(1_000_000_000);
    let nsecs = nanos.rem_euclid(1_000_000_000) as u32;
    (secs, nsecs)
}

fn apply_time_offset(value: Duration, offset_ns: i64) -> Duration {
    let adjusted = value.as_nanos() as i128 + offset_ns as i128;
    Duration::from_nanos(adjusted.clamp(0, u64::MAX as i128) as u64)
}
