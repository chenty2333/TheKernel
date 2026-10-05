//! Namespace-local Unix observations from live, weakly indexed file owners.
use super::*;

pub(crate) struct UnixSocketRecord {
    pub(crate) inode: u64,
    /// Native socket-wrapper owners, excluding this observation's temporary Arc.
    /// This is not a guessed Linux sk_refcnt or a transport-buffer reference count.
    pub(crate) owners: usize,
    pub(crate) snapshot: axnet::unix::UnixDiagnosticSnapshot,
}

pub(crate) fn unix_records(namespace: &Arc<NetworkNamespace>) -> AxResult<Vec<UnixSocketRecord>> {
    let namespace = Arc::downgrade(namespace);
    let entries = {
        let mut registry = SOCK_DIAG_REGISTRATIONS.lock();
        registry.retain(|entry| entry.strong_count() != 0);
        let mut entries = Vec::new();
        for entry in registry.iter().filter_map(Weak::upgrade) {
            if entry.family == 1 && Weak::ptr_eq(&entry.net_ns, &namespace) {
                entries.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                entries.push(entry);
            }
        }
        entries
    };
    let mut records = Vec::new();
    records
        .try_reserve(entries.len())
        .map_err(|_| AxError::NoMemory)?;
    for entry in entries {
        let Some(owner) = entry.owner.lock().clone().and_then(|owner| owner.upgrade()) else {
            continue;
        };
        let Some(socket) = owner.downcast_ref::<crate::file::Socket>() else {
            continue;
        };
        let axnet::Socket::Unix(unix) = &socket.inner else {
            continue;
        };
        records.push(UnixSocketRecord {
            inode: socket.diag_inode_owner().0,
            owners: Arc::strong_count(&owner).saturating_sub(1),
            snapshot: unix.diagnostic_snapshot(),
        });
    }
    Ok(records)
}
