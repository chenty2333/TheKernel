//! Task-owned immutable Landlock domain stack.
//!
//! This state is deliberately distinct from the boot-frozen LSM registry:
//! Landlock domains are created by userspace and snapshot on clone/fork.
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axerrno::{AxError, AxResult};
use axnet::unix::UnixEndpointIdentity;
use axsync::Mutex;
use spin::Lazy;

use super::{AuditLandlockDenied, emit_landlock_denial};
use crate::task::AsThread;

// Keep these aligned with Linux's LANDLOCK_ACCESS_FS_* UAPI values.  They are
// intentionally raw bits because rulesets retain the userspace ABI mask.
pub(crate) const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
pub(crate) const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
pub(crate) const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
pub(crate) const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
pub(crate) const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
pub(crate) const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
pub(crate) const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
pub(crate) const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;
pub(crate) const LANDLOCK_ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;
pub(crate) const LANDLOCK_ACCESS_NET_BIND_TCP: u64 = 1 << 0;
pub(crate) const LANDLOCK_ACCESS_NET_CONNECT_TCP: u64 = 1 << 1;
pub(crate) const LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
pub(crate) const LANDLOCK_SCOPE_SIGNAL: u64 = 1 << 1;

type AbstractUnixSocketKey = (usize, Vec<u8>);
const LANDLOCK_MAX_NUM_LAYERS: usize = 16;
pub(crate) const LANDLOCK_RESTRICT_SELF_LOG_SAME_EXEC_OFF: u32 = 1 << 0;
pub(crate) const LANDLOCK_RESTRICT_SELF_LOG_NEW_EXEC_ON: u32 = 1 << 1;
pub(crate) const LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF: u32 = 1 << 2;
pub(crate) const LANDLOCK_RESTRICT_SELF_LOG_MASK: u32 = LANDLOCK_RESTRICT_SELF_LOG_SAME_EXEC_OFF
    | LANDLOCK_RESTRICT_SELF_LOG_NEW_EXEC_ON
    | LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF;
static NEXT_LANDLOCK_DOMAIN_ID: AtomicU64 = AtomicU64::new(1);

/// Kernel-owned resolver adapter for an immutable Landlock ruleset.  The
/// syscall control plane implements this trait; task security intentionally
/// never reaches upward into that layer.
pub(crate) trait LandlockPolicy: Send + Sync {
    fn scoped(&self) -> u64;
    fn allows_path(&self, target: &axfs_ng_vfs::Location, access: u64) -> bool;
    fn allows_net_port(&self, port: u16, access: u64) -> bool;
    fn destination_is_no_less_restrictive(
        &self,
        source: &axfs_ng_vfs::Location,
        destination: &axfs_ng_vfs::Location,
        access: u64,
    ) -> bool;
}

/// Network operations do not have a VFS object to carry through the generic
/// permission path.  Keep their Landlock admission at the task-security
/// boundary, where the task-local domain and its audit lifecycle are stable.
pub(crate) fn check_current_landlock_net_port(port: u16, access: u64) -> AxResult {
    if let Some(current) = axtask::current_may_uninit()
        && let Some(thread) = current.try_as_thread()
    {
        return thread.landlock_domain().check_net_port(port, access);
    }
    Ok(())
}

fn check_landlock_layer_limit(layers: usize) -> AxResult<()> {
    if layers >= LANDLOCK_MAX_NUM_LAYERS {
        Err(axerrno::LinuxError::E2BIG.into())
    } else {
        Ok(())
    }
}

/// Abstract sockets are not VFS objects.  Retain their creator's task label
/// at bind time, keyed by the network namespace and abstract name, so later
/// connect/send admission observes the endpoint creator rather than the
/// current caller.
struct AbstractUnixSocketLabel {
    domain: LandlockDomain,
    endpoint: Mutex<Option<UnixEndpointIdentity>>,
    committed: AtomicBool,
}
static ABSTRACT_UNIX_SOCKET_LABELS: Lazy<
    Mutex<BTreeMap<AbstractUnixSocketKey, Arc<AbstractUnixSocketLabel>>>,
> = Lazy::new(|| Mutex::new(BTreeMap::new()));

pub(crate) struct AbstractUnixSocketLabelReservation {
    key: AbstractUnixSocketKey,
    label: Arc<AbstractUnixSocketLabel>,
}
impl AbstractUnixSocketLabelReservation {
    pub(crate) fn commit(&self, endpoint: UnixEndpointIdentity) {
        *self.label.endpoint.lock() = Some(endpoint);
        self.label.committed.store(true, Ordering::Release);
    }
}
impl Drop for AbstractUnixSocketLabelReservation {
    fn drop(&mut self) {
        let mut labels = ABSTRACT_UNIX_SOCKET_LABELS.lock();
        if labels
            .get(&self.key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.label))
        {
            labels.remove(&self.key);
        }
    }
}

pub(crate) fn reserve_abstract_unix_socket_label(
    net_namespace: usize,
    name: &[u8],
    domain: LandlockDomain,
) -> AxResult<AbstractUnixSocketLabelReservation> {
    let mut key_name = Vec::new();
    key_name
        .try_reserve_exact(name.len())
        .map_err(|_| AxError::NoMemory)?;
    key_name.extend_from_slice(name);
    let key = (net_namespace, key_name);
    let label = Arc::try_new(AbstractUnixSocketLabel {
        domain,
        endpoint: Mutex::new(None),
        committed: AtomicBool::new(false),
    })
    .map_err(|_| AxError::NoMemory)?;
    let mut labels = ABSTRACT_UNIX_SOCKET_LABELS.lock();
    if labels.contains_key(&key) {
        return Err(AxError::AddrInUse);
    }
    labels.insert(key.clone(), label.clone());
    Ok(AbstractUnixSocketLabelReservation { key, label })
}

/// Checks the label retained for the exact endpoint selected by an operation.
/// A current name mapping with another endpoint is deliberately denied rather
/// than consulted: close/rebind must not retarget the policy decision.
pub(crate) fn abstract_unix_socket_endpoint_is_in_scope(
    net_namespace: usize,
    name: &[u8],
    endpoint: UnixEndpointIdentity,
    actor: &LandlockDomain,
) -> bool {
    // A failed allocation while constructing this lookup key must not turn a
    // scoped IPC restriction into an allow.  Names are bounded by sockaddr.
    let mut key_name = Vec::new();
    if key_name.try_reserve_exact(name.len()).is_err() {
        return false;
    }
    key_name.extend_from_slice(name);
    ABSTRACT_UNIX_SOCKET_LABELS
        .lock()
        .get(&(net_namespace, key_name))
        .is_none_or(|target| {
            target.committed.load(Ordering::Acquire)
                && target.endpoint.lock().as_ref() == Some(&endpoint)
                && actor.allows_scope_to(&target.domain, LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET)
        })
}

struct LandlockLayer {
    ruleset: Arc<dyn LandlockPolicy>,
    /// Every restrict_self(2) application is a new hierarchy node, even when
    /// it reuses an existing ruleset FD.
    identity: Arc<()>,
    id: u64,
    denial_count: Arc<AtomicU64>,
    log_same_exec: bool,
    log_new_exec: bool,
    log_subdomains: bool,
    after_exec: bool,
}

#[derive(Clone, Default)]
pub(crate) struct LandlockDomain {
    stack: Vec<Arc<LandlockLayer>>,
}
impl LandlockDomain {
    pub(crate) fn push(&self, ruleset: Arc<dyn LandlockPolicy>, flags: u32) -> AxResult<Self> {
        check_landlock_layer_limit(self.stack.len())?;
        let mut stack = Vec::new();
        stack
            .try_reserve_exact(self.stack.len() + 1)
            .map_err(|_| AxError::NoMemory)?;
        stack.extend(self.stack.iter().cloned());
        stack.push(
            Arc::try_new(LandlockLayer {
                ruleset,
                identity: Arc::try_new(()).map_err(|_| AxError::NoMemory)?,
                id: NEXT_LANDLOCK_DOMAIN_ID.fetch_add(1, Ordering::Relaxed),
                denial_count: Arc::try_new(AtomicU64::new(0)).map_err(|_| AxError::NoMemory)?,
                log_same_exec: flags & LANDLOCK_RESTRICT_SELF_LOG_SAME_EXEC_OFF == 0,
                log_new_exec: flags & LANDLOCK_RESTRICT_SELF_LOG_NEW_EXEC_ON != 0,
                log_subdomains: flags & LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF == 0,
                after_exec: false,
            })
            .map_err(|_| AxError::NoMemory)?,
        );
        Ok(Self { stack })
    }
    pub(crate) fn mute_subdomains(&self) -> Self {
        Self {
            stack: self
                .stack
                .iter()
                .map(|layer| Self::copy_layer(layer, false, layer.after_exec))
                .collect(),
        }
    }
    pub(crate) fn after_exec(&self) -> Self {
        Self {
            stack: self
                .stack
                .iter()
                .map(|layer| Self::copy_layer(layer, layer.log_subdomains, true))
                .collect(),
        }
    }
    fn copy_layer(
        layer: &Arc<LandlockLayer>,
        log_subdomains: bool,
        after_exec: bool,
    ) -> Arc<LandlockLayer> {
        // ABI-7 logging state is credential-local.  Copy it before a task
        // changes it; ruleset identity and the denial counter stay shared.
        Arc::new(LandlockLayer {
            ruleset: layer.ruleset.clone(),
            identity: layer.identity.clone(),
            id: layer.id,
            denial_count: layer.denial_count.clone(),
            log_same_exec: layer.log_same_exec,
            log_new_exec: layer.log_new_exec,
            log_subdomains,
            after_exec,
        })
    }
    pub(crate) fn allows_path(&self, target: &axfs_ng_vfs::Location, access: u64) -> bool {
        self.stack
            .iter()
            .all(|layer| layer.ruleset.allows_path(target, access))
    }
    pub(crate) fn report_path_denial(&self, target: &axfs_ng_vfs::Location, access: u64) {
        self.report_denial(access, "path", |layer| {
            !layer.ruleset.allows_path(target, access)
        });
    }
    pub(crate) fn allows_net_port(&self, port: u16, access: u64) -> bool {
        self.stack
            .iter()
            .all(|layer| layer.ruleset.allows_net_port(port, access))
    }
    pub(crate) fn check_net_port(&self, port: u16, access: u64) -> AxResult {
        if self.allows_net_port(port, access) {
            return Ok(());
        }
        self.report_denial(access, "net", |layer| {
            !layer.ruleset.allows_net_port(port, access)
        });
        Err(AxError::PermissionDenied)
    }
    fn report_denial(
        &self,
        access: u64,
        blocker: &'static str,
        denies: impl Fn(&LandlockLayer) -> bool,
    ) {
        // The youngest denying layer is the one Linux attributes to this
        // request: exactly one increment and exactly one audit event.
        let Some((index, layer)) = self
            .stack
            .iter()
            .enumerate()
            .rev()
            .find(|(_, layer)| denies(layer))
        else {
            return;
        };
        layer.denial_count.fetch_add(1, Ordering::Relaxed);
        let nested = index + 1 != self.stack.len();
        let log = (!nested || layer.log_subdomains)
            && if layer.after_exec {
                layer.log_new_exec
            } else {
                layer.log_same_exec
            };
        if log {
            emit_landlock_denial(AuditLandlockDenied {
                domain_id: layer.id,
                access,
                blocker,
                on_exec: layer.after_exec,
            });
        }
    }
    pub(crate) fn destination_is_no_less_restrictive(
        &self,
        source: &axfs_ng_vfs::Location,
        destination: &axfs_ng_vfs::Location,
        access: u64,
    ) -> bool {
        self.stack.iter().all(|layer| {
            layer
                .ruleset
                .destination_is_no_less_restrictive(source, destination, access)
        })
    }

    /// A scoped layer can reach only the same hierarchy node or its children.
    /// Layers that did not request this scope impose no relationship check.
    pub(crate) fn allows_scope_to(&self, target: &Self, scope: u64) -> bool {
        self.stack.iter().enumerate().all(|(index, layer)| {
            layer.ruleset.scoped() & scope == 0
                || target
                    .stack
                    .get(index)
                    .is_some_and(|peer| Arc::ptr_eq(&layer.identity, &peer.identity))
        })
    }

    /// Ptrace establishes a parent/child debugger relationship, so it uses
    /// hierarchy ancestry directly rather than one of Landlock's IPC scopes.
    pub(crate) fn is_ancestor_of(&self, target: &Self) -> bool {
        self.stack.iter().enumerate().all(|(index, layer)| {
            target
                .stack
                .get(index)
                .is_some_and(|peer| Arc::ptr_eq(&layer.identity, &peer.identity))
        })
    }
}

#[cfg(test)]
mod tests {
    use axerrno::{AxError, LinuxError};

    use super::{LANDLOCK_MAX_NUM_LAYERS, check_landlock_layer_limit};

    #[test]
    fn seventeenth_landlock_layer_is_e2big() {
        assert_eq!(LANDLOCK_MAX_NUM_LAYERS, 16);
        assert_eq!(check_landlock_layer_limit(15), Ok(()));
        assert_eq!(
            check_landlock_layer_limit(16),
            Err(AxError::from(LinuxError::E2BIG))
        );
    }
}
