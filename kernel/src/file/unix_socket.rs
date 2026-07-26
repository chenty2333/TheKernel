use alloc::{string::String, sync::Arc};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{
    CreateDisposition, InitialNodeData, NamedCreateOptions, NodePermission, NodeType, path::Path,
};
use axnet::unix::{BindSlot, UnixBindReservation, UnixSocket, UnixSocketAddr, UnixSocketTarget};
use linux_raw_sys::general::{AT_FDCWD, IN_CREATE, W_OK};

use super::{
    permission::{
        NamedCreateTerminalType, SecurityFsContextExt, VfsSecurityContext,
        authorize_named_inode_create, check_create_permissions_with_frozen_metadata,
        check_open_permissions_with_security, initial_named_create_owner_mode_with_security,
    },
    validate_pathname, with_path_fs,
};
use crate::{mounts, time::wall_time};

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

fn check_bind_name_available(parent: &axfs_ng_vfs::Location, name: &str) -> AxResult<()> {
    match parent.lookup_no_follow_in_mount(name) {
        Ok(_) => Err(AxError::AlreadyExists),
        Err(AxError::NotFound) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Creates and binds a Linux pathname Unix socket with one frozen credential
/// view. Transport admission is private and reversible until the filesystem
/// backend has initialized the exact slot and atomically published the name.
pub(crate) fn bind_path(
    socket: &UnixSocket,
    path: Arc<String>,
    security: &VfsSecurityContext,
    requested_mode: NodePermission,
    umask: u32,
) -> AxResult<()> {
    let result = (|| {
        let path_ref = Path::new(path.as_ref());
        validate_pathname(path_ref)?;
        // This specialized socket/filesystem composite transaction is independent
        // of MutationTransaction: UnixBindReservation keeps transport admission
        // hidden and reversible, while the backend publishes fully initialized
        // inode data exactly once. The shared guard keeps final path admission,
        // writable-mount validation, and publication atomic against remount RO.
        let _mount_operation = mounts::namespace_operation();

        let (parent, name) = with_path_fs(AT_FDCWD, path_ref, |fs| {
            let (parent, name) = fs.resolve_named_create_security(
                path_ref,
                security,
                NamedCreateTerminalType::NonDirectory,
            )?;
            // Linux maps every pre-existing final component (including a
            // symlink, non-socket inode, trailing-slash target, or covered
            // mountpoint) to EADDRINUSE. Keep this exact lookup in the parent
            // mount so a mounted root cannot replace the covered inode seen by
            // the subsequent exclusive create.
            check_bind_name_available(&parent, name)?;
            Ok((parent, try_owned_name(name)?))
        })?;
        let parent_metadata = parent.metadata()?;
        check_create_permissions_with_frozen_metadata(&parent, &parent_metadata, security)?;
        if !parent.supports_named_create(NodeType::Socket) {
            return Err(AxError::OperationNotPermitted);
        }
        let (mode, owner) = initial_named_create_owner_mode_with_security(
            &parent_metadata,
            security,
            NodeType::Socket,
            requested_mode,
            umask,
        );
        authorize_named_inode_create(
            &parent,
            &parent_metadata,
            &name,
            NodeType::Socket,
            mode,
            None,
            security,
        )?;

        let slot = Arc::try_new(BindSlot::default()).map_err(|_| AxError::NoMemory)?;
        let target = UnixSocketTarget::new(UnixSocketAddr::Path(path), slot.clone())?;
        let reservation: UnixBindReservation<'_> = socket.reserve_bind(target)?;
        let initial_data = InitialNodeData::from_shared(slot);

        let location = parent.create_named(
            &name,
            &NamedCreateOptions {
                node_type: NodeType::Socket,
                permission: mode,
                owner: Some(owner),
                rdev: None,
                initial_data: Some(initial_data),
            },
            CreateDisposition::Exclusive,
        )?;

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
    })();
    result.map_err(map_bind_create_error)
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
    security: &VfsSecurityContext,
) -> AxResult<()> {
    let path_ref = Path::new(path.as_ref());
    validate_pathname(path_ref)?;
    let location = with_path_fs(AT_FDCWD, path_ref, |fs| {
        fs.resolve_no_follow_security(path_ref, security)
    })?;
    check_open_permissions_with_security(
        &location,
        W_OK,
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
    )?;
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
    security: &VfsSecurityContext,
) -> AxResult<UnixSocketTarget> {
    let path_ref = Path::new(path.as_ref());
    validate_pathname(path_ref)?;
    let location = with_path_fs(AT_FDCWD, path_ref, |fs| {
        fs.resolve_security(path_ref, security)
    })?;
    check_open_permissions_with_security(
        &location,
        W_OK,
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
    )?;
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
    use axfs_ng_vfs::{Mountpoint, NodeType};

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

    #[test]
    fn bind_availability_checks_the_exact_covered_name() {
        let filesystem = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let root = mount.root_location();
        root.create(
            "file",
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        )
        .unwrap();
        root.create_symlink(
            "symlink",
            "target",
            NodePermission::from_bits_truncate(0o777),
            Some((0, 0)),
        )
        .unwrap();
        let covered = root
            .create(
                "covered",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o700),
            )
            .unwrap();
        let child_filesystem = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        covered.mount(&child_filesystem).unwrap();

        assert_eq!(check_bind_name_available(&root, "missing"), Ok(()));
        for name in ["file", "symlink", "covered"] {
            let error = check_bind_name_available(&root, name).unwrap_err();
            assert_eq!(error, AxError::AlreadyExists);
            assert_eq!(map_bind_create_error(error), AxError::AddrInUse);
        }
        assert!(root.lookup_no_follow("covered").unwrap().is_root_of_mount());
        assert!(
            root.lookup_no_follow_in_mount("covered")
                .unwrap()
                .same_node(&covered)
        );
    }
}
