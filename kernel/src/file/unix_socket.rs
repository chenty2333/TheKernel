use alloc::{string::String, sync::Arc};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{
    CreateDisposition, InitialNodeData, NamedCreateOptions, NodePermission, NodeType, path::Path,
};
use axnet::unix::{BindSlot, UnixBindReservation, UnixSocket, UnixSocketAddr, UnixSocketTarget};
use linux_raw_sys::general::{AT_FDCWD, IN_CREATE, W_OK};

use super::{
    permission::{
        DacFsContextExt, check_create_permissions, check_open_permissions,
        initial_named_create_owner_mode,
    },
    validate_pathname, with_path_fs,
};
use crate::{mounts, task::DacCredentialView, time::wall_time};

fn try_owned_name(name: &str) -> AxResult<String> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(name.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.push_str(name);
    Ok(owned)
}

#[cfg(feature = "dev-log")]
pub(crate) fn try_path(path: &str) -> AxResult<Arc<String>> {
    Arc::try_new(try_owned_name(path)?).map_err(|_| AxError::NoMemory)
}

fn map_bind_create_error(error: AxError) -> AxError {
    if error == AxError::AlreadyExists {
        AxError::AddrInUse
    } else {
        error
    }
}

/// Creates and binds a Linux pathname Unix socket with one frozen credential
/// view. Transport admission is private and reversible until the filesystem
/// backend has initialized the exact slot and atomically published the name.
pub(crate) fn bind_path(
    socket: &UnixSocket,
    path: Arc<String>,
    credentials: &DacCredentialView,
    requested_mode: NodePermission,
    umask: u32,
) -> AxResult<()> {
    let path_ref = Path::new(path.as_ref());
    validate_pathname(path_ref)?;

    // Linux maps any pre-existing final component (including a symlink or a
    // non-socket inode) to EADDRINUSE. The exclusive backend create below is
    // still authoritative for a concurrent creator.
    let existing = with_path_fs(AT_FDCWD, path_ref, |fs| {
        match fs.resolve_no_follow_dac(path_ref, credentials) {
            Ok(_) => Ok(true),
            Err(AxError::NotFound) => Ok(false),
            Err(error) => Err(error),
        }
    })?;
    if existing {
        return Err(AxError::AddrInUse);
    }

    let (parent, name) = with_path_fs(AT_FDCWD, path_ref, |fs| {
        let (parent, name) = fs.resolve_nonexistent_dac(path_ref, credentials)?;
        check_create_permissions(&parent, credentials)?;
        Ok((parent, try_owned_name(name)?))
    })?;
    let (mode, owner) = initial_named_create_owner_mode(
        &parent.metadata()?,
        credentials,
        NodeType::Socket,
        requested_mode,
        umask,
    );

    let slot = Arc::try_new(BindSlot::default()).map_err(|_| AxError::NoMemory)?;
    let target = UnixSocketTarget::new(UnixSocketAddr::Path(path), slot.clone())?;
    let reservation: UnixBindReservation<'_> = socket.reserve_bind(target)?;
    let initial_data = InitialNodeData::from_shared(slot);

    let location = parent
        .create_named(
            &name,
            &NamedCreateOptions {
                node_type: NodeType::Socket,
                permission: mode,
                owner: Some(owner),
                rdev: None,
                initial_data: Some(initial_data),
            },
            CreateDisposition::Exclusive,
        )
        .map_err(AxError::from)
        .map_err(map_bind_create_error)?;

    // No fallible work is permitted after the backend makes the initialized
    // name visible. Committing only moves/clones already admitted ownership.
    reservation.commit_with_keepalive(location.entry.entry().lifetime_token());
    if let Err(error) = crate::file::inotify::notify_parent_with_name(
        &parent,
        Some(&location.entry),
        location.entry.name(),
        IN_CREATE,
        false,
        0,
    ) {
        warn!("Unix socket create notification failed: {error}");
    }
    Ok(())
}

/// Binds a kernel-owned endpoint to a socket inode populated by a static
/// pseudo-filesystem builder.
///
/// This is deliberately separate from Linux `bind(2)`: userspace must create
/// a new pathname and receives `EADDRINUSE` for every pre-existing inode. Some
/// kernel pseudo-filesystems, however, publish their complete directory tree
/// before runtime services start and do not support named creation. The exact
/// inode owns the transport slot, so namespace lookup remains VFS-driven.
#[cfg(feature = "dev-log")]
pub(crate) fn bind_precreated_path(
    socket: &UnixSocket,
    path: Arc<String>,
    credentials: &DacCredentialView,
) -> AxResult<()> {
    let path_ref = Path::new(path.as_ref());
    validate_pathname(path_ref)?;
    let location = with_path_fs(AT_FDCWD, path_ref, |fs| {
        fs.resolve_no_follow_dac(path_ref, credentials)
    })?;
    check_open_permissions(&location, W_OK, credentials)?;
    if location.metadata()?.node_type != NodeType::Socket {
        return Err(AxError::AddrInUse);
    }

    let slot = {
        let mut data = location.user_data();
        data.try_get_or_insert_with(BindSlot::default)?
    };
    let target = UnixSocketTarget::new(UnixSocketAddr::Path(path), slot)?;
    let reservation = socket.reserve_bind(target)?;
    reservation.commit_with_keepalive(location.entry().lifetime_token());
    Ok(())
}

/// Resolves a Linux pathname Unix peer and enforces path-search plus socket
/// inode write permission using one frozen credential view.
pub(crate) fn resolve_peer(
    path: Arc<String>,
    credentials: &DacCredentialView,
) -> AxResult<UnixSocketTarget> {
    let path_ref = Path::new(path.as_ref());
    validate_pathname(path_ref)?;
    let location = with_path_fs(AT_FDCWD, path_ref, |fs| {
        fs.resolve_dac(path_ref, credentials)
    })?;
    check_open_permissions(&location, W_OK, credentials)?;
    if location.metadata()?.node_type != NodeType::Socket {
        return Err(AxError::ConnectionRefused);
    }
    let slot = location
        .user_data()
        .get::<BindSlot>()
        .ok_or(AxError::ConnectionRefused)?;

    // Linux touches atime after resolving a usable pathname socket. Failure to
    // update atime does not invalidate the already admitted connection.
    if mounts::should_update_atime(&location) {
        let _ = location.update_metadata(axfs_ng_vfs::MetadataUpdate {
            atime: Some(wall_time()),
            ..Default::default()
        });
    }
    UnixSocketTarget::from_bound(slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_name_is_reported_as_address_in_use() {
        assert_eq!(
            map_bind_create_error(AxError::AlreadyExists),
            AxError::AddrInUse
        );
        assert_eq!(
            map_bind_create_error(AxError::PermissionDenied),
            AxError::PermissionDenied
        );
    }
}
