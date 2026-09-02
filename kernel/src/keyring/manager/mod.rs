//! The key manager: one lock-protected object owning every key, keyring,
//! and per-owner quota ledger in the kernel.
//!
//! The manager is split by operation family: [`gc`] plans and commits
//! bounded retirement transactions, [`objects`] constructs keys and
//! per-task root slots, [`lifecycle`] handles fork/exec/exit, [`resolve`]
//! resolves special keyrings and permissions, [`links`] mutates the link
//! graph, [`search`] implements search and persistent keyrings, and
//! [`syscall`] is the `add_key`/`request_key`/`keyctl` surface.

mod gc;
mod lifecycle;
mod links;
mod objects;
mod resolve;
mod search;
mod syscall;
#[cfg(test)]
mod tests;

use alloc::{
    collections::{BTreeMap, BTreeSet},
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
#[cfg(test)]
use core::mem::size_of;
use core::ops::Bound::{Excluded, Unbounded};

use axerrno::{AxError, AxResult, LinuxError};
use thekernel_linux_cred::{KeyPermission, KeyPermissionMask};

#[cfg(test)]
use super::accounting::{
    KEY_MAXBYTES_DEFAULT, KEY_MAXKEYS_DEFAULT, MANAGER_MAX_LINK_BYTES, ManagerBudgetLimits,
    ManagerBudgetUsage, OwnerUsage, validate_key_quota_limit,
};
#[cfg(test)]
use super::object::{KEY_RESIDENT_NODE_OVERHEAD, permission_mask};
use super::{
    accounting::{
        AbiQuotaCharge, AccountingPlan, ManagerBudget, OwnerLedger, QuotaAdmission, ResidentCharge,
        user_maxbytes, user_maxkeys,
    },
    contract::{KeyActor, KeyTaskOwner, KeyUserRecord, KeyctlCommand, KeyctlOutput, ReqKeyDefault},
    object::{
        BIG_KEY_ABI_PAYLOAD_CHARGE, GcPlanScratch, KEY_LINK_CHARGE, Key, KeyState, KeyTypeKind,
        PublishedKeyringName, anonymous_session_keyring_permissions,
        named_session_keyring_permissions, persistent_keyring_permissions,
        thread_process_keyring_permissions, uid_keyring_permissions, wipe_key_bytes,
    },
};
#[cfg(test)]
use crate::task::{Credentials, DacCredentialView};
use crate::{
    task::{Kgid, Kuid, UserNamespace, UserNamespaceId},
    time::wall_time,
};

pub(super) const KEY_SPEC_THREAD_KEYRING: i32 = -1;
const KEY_SPEC_PROCESS_KEYRING: i32 = -2;
pub(super) const KEY_SPEC_SESSION_KEYRING: i32 = -3;
const KEY_SPEC_USER_KEYRING: i32 = -4;
const KEY_SPEC_USER_SESSION_KEYRING: i32 = -5;

const KEYRING_SEARCH_MAX_DEPTH: usize = 6;
const PERSISTENT_KEYRING_TIMEOUT_SECS: u64 = 3 * 24 * 60 * 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PossessionContext {
    Recompute,
    Fixed(bool),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResolvedKey {
    serial: i32,
    possession: PossessionContext,
}

impl ResolvedKey {
    const fn numeric(serial: i32) -> Self {
        Self {
            serial,
            possession: PossessionContext::Recompute,
        }
    }

    const fn possessed(serial: i32) -> Self {
        Self {
            serial,
            possession: PossessionContext::Fixed(true),
        }
    }

    const fn with_possession(serial: i32, possessed: bool) -> Self {
        Self {
            serial,
            possession: PossessionContext::Fixed(possessed),
        }
    }
}

impl From<i32> for ResolvedKey {
    fn from(serial: i32) -> Self {
        Self::numeric(serial)
    }
}

pub(super) struct KeyManager {
    next_serial: i32,
    next_name_order: u64,
    next_gc_epoch: u64,
    keys: BTreeMap<i32, Key>,
    owners: OwnerLedger,
    budget: ManagerBudget,
    thread_keyrings: BTreeMap<u32, i32>,
    process_keyrings: BTreeMap<u32, i32>,
    session_keyrings: BTreeMap<u32, i32>,
    reqkey_defaults: BTreeMap<u32, i32>,
    /// Per-thread, one-shot construction authority installed by request_key.
    /// The serial is also recorded in the key, so task teardown can revoke an
    /// abandoned construction without granting it to a reused visible TID.
    construction_authorities: BTreeMap<u32, i32>,
    /// In-flight request_key constructions, scoped exactly as Linux key
    /// lookup is scoped.  Pending keys are intentionally not linked into a
    /// requester's keyrings, so ordinary lookup cannot be used to coalesce a
    /// second request for the same object.
    pending_constructions: BTreeMap<PendingConstructionKey, i32>,
    namespaces: BTreeMap<UserNamespaceId, NamespaceRegistry>,
    #[cfg(test)]
    namespace_ensure_calls: usize,
    #[cfg(test)]
    namespace_prune_candidates: usize,
}

/// A construction which has been made visible only to the key service.  The
/// requester never receives construction authority: that one-shot authority
/// is transferred to the dedicated request-key helper after its process has
/// been published.
pub(crate) struct RequestKeyConstruction {
    pub(crate) serial: i32,
    pub(crate) kind: KeyTypeKind,
    pub(crate) description: String,
    pub(crate) callout: String,
}

/// Identity of one in-flight construction.  The type name is stable and
/// avoids making the manager's internal index depend on a formatting or ABI
/// representation of `KeyTypeKind`.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PendingConstructionKey {
    namespace: UserNamespaceId,
    kind: &'static str,
    description: String,
}

impl PendingConstructionKey {
    fn new(namespace: UserNamespaceId, kind: KeyTypeKind, description: &str) -> Self {
        Self {
            namespace,
            kind: kind.name(),
            description: description.to_string(),
        }
    }
}

pub(crate) enum RequestKeyBegin {
    Resolved(isize),
    /// Another requester already owns helper creation.  The service waits on
    /// this serial and rechecks the terminal state in its own namespace.
    Pending(i32),
    Construction(RequestKeyConstruction),
}

/// Keyring state whose lifetime and lookup domain are one user namespace.
///
/// The weak namespace handle is deliberate: the key manager must not keep a
/// namespace alive merely because task-exit hooks have not yet retired every
/// cached root. The next manager operation prunes dead registries and releases
/// their root references under the existing service mutex.
struct NamespaceRegistry {
    namespace: Weak<UserNamespace>,
    user_keyrings: BTreeMap<Kuid, i32>,
    user_session_keyrings: BTreeMap<Kuid, i32>,
    persistent_keyrings: BTreeMap<Kuid, i32>,
}

impl NamespaceRegistry {
    fn new(namespace: &Arc<UserNamespace>) -> Self {
        Self {
            namespace: Arc::downgrade(namespace),
            user_keyrings: BTreeMap::new(),
            user_session_keyrings: BTreeMap::new(),
            persistent_keyrings: BTreeMap::new(),
        }
    }

    fn root_serials(&self) -> impl Iterator<Item = &i32> {
        self.user_keyrings
            .values()
            .chain(self.user_session_keyrings.values())
            .chain(self.persistent_keyrings.values())
    }

    fn detach_serial(&mut self, serial: i32) {
        self.user_keyrings.retain(|_, linked| *linked != serial);
        self.user_session_keyrings
            .retain(|_, linked| *linked != serial);
        self.persistent_keyrings
            .retain(|_, linked| *linked != serial);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RootSource {
    Thread(u32),
    Process(u32),
    Session(u32),
    User(UserNamespaceId, Kuid),
    UserSession(UserNamespaceId, Kuid),
    Persistent(UserNamespaceId, Kuid),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExpectedTaskRoot {
    source: RootSource,
    serial: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreparedGcRoots {
    Namespace(UserNamespaceId),
    Task([Option<ExpectedTaskRoot>; 3]),
}

struct GcTxnBuild {
    epoch: u64,
    roots: PreparedGcRoots,
    touched_head: Option<i32>,
    touched_count: usize,
    work_head: Option<i32>,
    owner_head: Option<Kuid>,
    owner_count: usize,
    retired: ResidentCharge,
}

#[must_use = "a prepared key GC transaction must be committed under the manager lock"]
struct PreparedGcTxn {
    epoch: u64,
    roots: PreparedGcRoots,
    touched_head: Option<i32>,
    touched_count: usize,
    owner_head: Option<Kuid>,
    owner_count: usize,
    budget_after: super::accounting::ManagerBudgetUsage,
}

/// Exact child-owned state installed while clone construction is still
/// private. The service façade retains this value until TASK_TABLE publication
/// succeeds, then either disarms it or uses it for allocation-free rollback.
pub(super) struct ForkUndo {
    child: KeyTaskOwner,
    thread_keyring: Option<i32>,
    session_keyring: Option<i32>,
    reqkey_default: Option<i32>,
}

impl KeyManager {
    pub(super) const fn new() -> Self {
        Self {
            next_serial: 1,
            next_name_order: 1,
            next_gc_epoch: 1,
            keys: BTreeMap::new(),
            owners: OwnerLedger {
                usage: BTreeMap::new(),
            },
            budget: ManagerBudget::kernel_default(),
            thread_keyrings: BTreeMap::new(),
            process_keyrings: BTreeMap::new(),
            session_keyrings: BTreeMap::new(),
            reqkey_defaults: BTreeMap::new(),
            construction_authorities: BTreeMap::new(),
            pending_constructions: BTreeMap::new(),
            namespaces: BTreeMap::new(),
            #[cfg(test)]
            namespace_ensure_calls: 0,
            #[cfg(test)]
            namespace_prune_candidates: 0,
        }
    }

    fn alloc_serial(&mut self) -> AxResult<i32> {
        let start = self.next_serial.max(1);
        let mut serial = start;
        loop {
            if !self.keys.contains_key(&serial) {
                self.next_serial = serial.checked_add(1).unwrap_or(1);
                return Ok(serial);
            }
            serial = serial.checked_add(1).unwrap_or(1);
            if serial == start {
                return Err(LinuxError::ENOSPC.into());
            }
        }
    }

    fn plan_name_publication(
        &self,
        serial: i32,
        namespace: UserNamespaceId,
    ) -> AxResult<(PublishedKeyringName, u64)> {
        let key = self.keys.get(&serial).ok_or(AxError::BadState)?;
        if !key.is_keyring() || key.published_name.is_some() {
            return Err(AxError::BadState);
        }
        let next = self
            .next_name_order
            .checked_add(1)
            .ok_or(AxError::from(LinuxError::ENOSPC))?;
        Ok((
            PublishedKeyringName {
                namespace,
                order: self.next_name_order,
            },
            next,
        ))
    }

    /// Commits a previously validated publication after its owning link/root
    /// has become durable under the same manager mutex.
    fn commit_name_publication(
        &mut self,
        serial: i32,
        publication: PublishedKeyringName,
        next: u64,
    ) {
        debug_assert_eq!(publication.order, self.next_name_order);
        let key = self
            .keys
            .get_mut(&serial)
            .expect("planned keyring name lost before publication");
        debug_assert!(key.is_keyring() && key.published_name.is_none());
        key.published_name = Some(publication);
        self.next_name_order = next;
    }

    #[cfg(test)]
    fn with_budget(limits: ManagerBudgetLimits) -> Self {
        Self {
            budget: ManagerBudget::new(limits),
            ..Self::new()
        }
    }
}
