use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::FsContext;
use axfs_ng_vfs::{
    CreateDisposition, FsName, FsNameBuf, FsPath, InitialNodeData, NamedCreateOptions,
    NodePermission, NodeType,
};
use axnet::unix::{BindSlot, UnixBindReservation, UnixSocket, UnixSocketAddr, UnixSocketTarget};
use linux_raw_sys::general::{AT_FDCWD, IN_CREATE, W_OK};

use super::{
    permission::{
        NamedCreateTerminalType, SecurityFsContextExt, VfsSecurityContext,
        authorize_named_inode_create, check_create_permissions_with_frozen_metadata,
        check_open_permissions_with_security, initial_named_create_owner_mode_with_security_at,
    },
    posix_acl, validate_pathname, with_path_fs,
};
use crate::{mounts, time::wall_time};

fn try_owned_name(name: &FsName) -> AxResult<FsNameBuf> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(name.as_bytes().len())
        .map_err(|_| AxError::NoMemory)?;
    owned.extend_from_slice(name.as_bytes());
    FsNameBuf::from_vec(owned).map_err(Into::into)
}

fn map_bind_create_error(error: AxError) -> AxError {
    if error == AxError::AlreadyExists {
        AxError::AddrInUse
    } else {
        error
    }
}

fn check_bind_name_available(parent: &axfs_ng_vfs::Location, name: &FsName) -> AxResult<()> {
    match parent.lookup_no_follow_in_mount(name) {
        Ok(_) => Err(AxError::AlreadyExists),
        Err(AxError::NotFound) => Ok(()),
        Err(error) => Err(error),
    }
}

/// The `sk_type` of a pathname UNIX socket.
///
/// Linux `unix_find_bsd()` compares the connecting socket's type with the
/// bound socket's type before it runs `security_unix_find()`, so a mismatch is
/// `-EPROTOTYPE` rather than a Landlock `-EACCES`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnixPeerKind {
    Stream,
    Datagram,
    Seqpacket,
}

impl UnixPeerKind {
    pub(crate) fn of(socket: &UnixSocket) -> Self {
        if socket.is_datagram() {
            Self::Datagram
        } else if socket.is_seqpacket() {
            Self::Seqpacket
        } else {
            Self::Stream
        }
    }
}

/// Filesystem-node payload of one bound pathname UNIX socket.
///
/// Linux keeps the creating socket file's `f_cred` alive in `struct socket`, so
/// `hook_unix_find()` can compare the connecting domain with the domain of the
/// process that created the listening socket.  The kernel-side socket object
/// already retains that snapshot (`Socket::creator_security`); the inode this
/// module creates carries it for as long as the name stays resolvable, so a
/// later `close`, rename, or unlink cannot retarget the comparison.
struct PathnameSocketData {
    slot: Arc<BindSlot>,
    creator_domain: crate::task::security::LandlockDomain,
    kind: UnixPeerKind,
}

fn pathname_socket_data(location: &axfs_ng_vfs::Location) -> Option<Arc<PathnameSocketData>> {
    location.user_data().get::<PathnameSocketData>()
}

/// Creates and binds a Linux pathname Unix socket with one frozen credential
/// view. Transport admission is private and reversible until the filesystem
/// backend has initialized the exact slot and atomically published the name.
pub(crate) fn bind_path<G>(
    socket: &UnixSocket,
    path: Arc<Vec<u8>>,
    security: &VfsSecurityContext,
    creator_domain: crate::task::security::LandlockDomain,
    requested_mode: NodePermission,
    umask: u32,
    prepare: impl FnOnce(axnet::unix::UnixEndpointIdentity) -> AxResult<G>,
) -> AxResult<G> {
    let lookup_path = path.clone();
    let result = with_path_fs(
        AT_FDCWD,
        FsPath::new(lookup_path.as_slice()),
        |fs| {
            bind_path_in_fs(
                fs,
                path,
                socket,
                security,
                creator_domain,
                requested_mode,
                umask,
                prepare,
            )
        },
    );
    result.map_err(map_bind_create_error)
}

fn bind_path_in_fs<G>(
    fs: &FsContext,
    path: Arc<Vec<u8>>,
    socket: &UnixSocket,
    security: &VfsSecurityContext,
    creator_domain: crate::task::security::LandlockDomain,
    requested_mode: NodePermission,
    umask: u32,
    prepare: impl FnOnce(axnet::unix::UnixEndpointIdentity) -> AxResult<G>,
) -> AxResult<G> {
    let path_ref = FsPath::new(path.as_slice());
    validate_pathname(path_ref)?;
    // This specialized socket/filesystem composite transaction is independent
    // of MutationTransaction: UnixBindReservation keeps transport admission
    // hidden and reversible, while the backend publishes fully initialized
    // inode data exactly once. The shared guard keeps final path admission,
    // writable-mount validation, and publication atomic against remount RO.
    let _mount_operation = mounts::namespace_operation();

    let (parent, name) = fs.resolve_named_create_security(
        path_ref,
        security,
        NamedCreateTerminalType::NonDirectory,
    )?;
    // Linux maps every pre-existing final component (including a symlink,
    // non-socket inode, trailing-slash target, or covered mountpoint) to
    // EADDRINUSE. Keep this exact lookup in the parent mount so a mounted root
    // cannot replace the covered inode seen by the subsequent exclusive create.
    check_bind_name_available(&parent, name)?;
    let name = try_owned_name(name)?;
    let parent_metadata = parent.metadata()?;
    check_create_permissions_with_frozen_metadata(&parent, &parent_metadata, security)?;
    if !parent.supports_named_create(NodeType::Socket) {
        return Err(AxError::OperationNotPermitted);
    }
    let (mut mode, owner) = initial_named_create_owner_mode_with_security_at(
        &parent,
        &parent_metadata,
        security,
        NodeType::Socket,
        requested_mode,
        umask,
    )?;
    if let Some(default_mode) = posix_acl::initial_mode(&parent, requested_mode)? {
        mode = NodePermission::from_bits_truncate(
            (mode.bits() & !0o777) | (default_mode.bits() & 0o777),
        );
    }
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
    let endpoint = reservation.target_endpoint_identity()?;
    let prepared = prepare(endpoint)?;
    // The payload is allocated before the name becomes visible: publication
    // may not fail, and a later resolver must observe the creator's domain.
    let initial_data = InitialNodeData::from_shared(
        Arc::try_new(PathnameSocketData {
            slot,
            creator_domain,
            kind: UnixPeerKind::of(socket),
        })
        .map_err(|_| AxError::NoMemory)?,
    );
    let (project_id, project_inherit) =
        super::inode_flags::prepare_inherited_project_id(&parent, false)?;
    let (access_acl, default_acl) =
        super::posix_acl::prepare_inherited_default(&parent, NodeType::Socket, mode)?;

    let location = parent.create_named(
        &name,
        &NamedCreateOptions {
            node_type: NodeType::Socket,
            permission: mode,
            owner: Some(owner),
            rdev: None,
            initial_data: Some(initial_data),
            initial_attributes: axfs_ng_vfs::PreparedInitialAttributes {
                project_id,
                project_inherit,
                access_acl,
                default_acl,
            },
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
    Ok(prepared)
}

/// Resolves a Linux pathname Unix peer and enforces path-search plus socket
/// inode write permission using one frozen credential view.
pub(crate) fn resolve_peer(
    path: Arc<Vec<u8>>,
    security: &VfsSecurityContext,
    kind: UnixPeerKind,
) -> AxResult<UnixSocketTarget> {
    let lookup_path = path.clone();
    with_path_fs(
        AT_FDCWD,
        FsPath::new(lookup_path.as_slice()),
        |fs| resolve_peer_in_fs(fs, path, security, kind),
    )
}

fn resolve_peer_in_fs(
    fs: &FsContext,
    path: Arc<Vec<u8>>,
    security: &VfsSecurityContext,
    kind: UnixPeerKind,
) -> AxResult<UnixSocketTarget> {
    let path_ref = FsPath::new(path.as_slice());
    validate_pathname(path_ref)?;
    let location = fs.resolve_security(path_ref, security)?;
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
    let data = pathname_socket_data(&location).ok_or(AxError::ConnectionRefused)?;
    // Linux `unix_find_bsd()` checks `sk->sk_type != type` (EPROTOTYPE) before
    // it reaches `security_unix_find()`.
    if data.kind != kind {
        return Err(LinuxError::EPROTOTYPE.into());
    }
    // Linux `security_unix_find()` runs inside `unix_find_bsd()` after the
    // socket inode is found and the transport type matches, and before atime
    // is touched, so a non-socket or unreachable peer keeps ECONNREFUSED and
    // a Landlock denial is EACCES.
    if let Some(domain) = security.landlock_domain() {
        domain.check_unix_socket_resolution(&location, Some(&data.creator_domain))?;
    }

    // Linux touches atime after resolving a usable pathname socket. Failure to
    // update atime does not invalidate the already admitted connection.
    if mounts::should_update_atime(&location) {
        let _ = location.update_metadata(axfs_ng_vfs::MetadataUpdate {
            atime: Some(wall_time().into()),
            ..Default::default()
        });
    }
    UnixSocketTarget::from_bound(data.slot.clone())
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
            axfs_ng_vfs::FsName::new(b"file"),
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        )
        .unwrap();
        root.create_symlink(
            axfs_ng_vfs::FsName::new(b"symlink"),
            axfs_ng_vfs::FsPath::new(b"target"),
            NodePermission::from_bits_truncate(0o777),
            Some((0, 0)),
        )
        .unwrap();
        let covered = root
            .create(
                axfs_ng_vfs::FsName::new(b"covered"),
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o700),
            )
            .unwrap();
        let child_filesystem = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        covered.mount(&child_filesystem).unwrap();

        assert_eq!(check_bind_name_available(&root, axfs_ng_vfs::FsName::new(b"missing")), Ok(()));
        for name in ["file", "symlink", "covered"] {
            let error = check_bind_name_available(&root, axfs_ng_vfs::FsName::new(name.as_bytes())).unwrap_err();
            assert_eq!(error, AxError::AlreadyExists);
            assert_eq!(map_bind_create_error(error), AxError::AddrInUse);
        }
        assert!(root.lookup_no_follow(axfs_ng_vfs::FsName::new(b"covered")).unwrap().is_root_of_mount());
        assert!(
            root.lookup_no_follow_in_mount(axfs_ng_vfs::FsName::new(b"covered"))
                .unwrap()
                .same_node(&covered)
        );
    }
    #[test]
    fn devfs_bind_unlink_rebind_preserves_exact_transport_identity() {
        use axnet::{RecvFlags, RecvOptions, SendFlags, SendOptions, SocketAddrEx, SocketOps};
        use axnet::unix::{DgramTransport, UnixNamespace, UnixSocketAddr};
        use crate::task::{Cred, UserNamespace};

        let filesystem = crate::pseudofs::dev::new_test_devfs();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let root = mount.root_location();
        let fs = FsContext::new(root.clone());
        let security = VfsSecurityContext::new(
            Cred::try_root(UserNamespace::try_new_root().unwrap()).unwrap(),
        );
        let namespace = UnixNamespace::try_new().unwrap();
        let make_socket = || UnixSocket::new(DgramTransport::new().unwrap(), namespace.clone());
        let old_server = make_socket();
        let new_server = make_socket();
        let client = make_socket();
        let path = Arc::new(b"/log".to_vec());
        let mode = NodePermission::from_bits_truncate(0o666);
        let creator = crate::task::security::LandlockDomain::default();
        assert!(matches!(
            bind_path_in_fs(
                &fs,
                path.clone(),
                &old_server,
                &security,
                creator.clone(),
                mode,
                0,
                |_| Err::<(), _>(AxError::NoMemory)
            ),
            Err(AxError::NoMemory)
        ));
        assert!(matches!(root.lookup_no_follow(FsName::new(b"log")), Err(AxError::NotFound)));
        let mut published = None;
        bind_path_in_fs(
            &fs,
            path.clone(),
            &old_server,
            &security,
            creator.clone(),
            mode,
            0,
            |identity| {
                published = Some(identity);
                Ok(())
            },
        )
        .unwrap();
        let old_target = resolve_peer_in_fs(&fs, path.clone(), &security, UnixPeerKind::Datagram).unwrap();
        assert_eq!(published, Some(old_target.endpoint_identity().unwrap()));
        let old_node = root.lookup_no_follow(FsName::new(b"log")).unwrap();
        assert_eq!(old_node.metadata().unwrap().node_type, NodeType::Socket);

        let exchange = |target: UnixSocketTarget, server: &UnixSocket, byte: u8| {
            assert_eq!(client.send_to_resolved(&[byte][..], SendOptions {
                flags: SendFlags::DONT_WAIT,
                to: Some(SocketAddrEx::Unix(UnixSocketAddr::Path(path.clone()))),
                ..Default::default()
            }, target), Ok(1));
            let mut received = [0u8];
            assert_eq!(server.recv(&mut received[..], RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ..Default::default()
            }), Ok(1));
            assert_eq!(received, [byte]);
        };
        exchange(old_target.clone(), &old_server, 1);
        root.unlink(FsName::new(b"log"), false).unwrap();
        assert!(matches!(resolve_peer_in_fs(&fs, path.clone(), &security, UnixPeerKind::Datagram),
            Err(AxError::NotFound)));
        exchange(old_target.clone(), &old_server, 2);

        bind_path_in_fs(&fs, path.clone(), &new_server, &security, creator, mode, 0, |_| Ok(()))
            .unwrap();
        let new_target = resolve_peer_in_fs(&fs, path.clone(), &security, UnixPeerKind::Datagram).unwrap();
        let new_node = root.lookup_no_follow(FsName::new(b"log")).unwrap();
        assert!(!old_node.same_node(&new_node));
        assert_ne!(old_target.endpoint_identity().unwrap(), new_target.endpoint_identity().unwrap());
        exchange(new_target, &new_server, 3);
        exchange(old_target, &old_server, 4);
        for server in [&old_server, &new_server] {
            assert_eq!(server.recv(&mut [0u8][..], RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ..Default::default()
            }), Err(AxError::WouldBlock));
        }
    }

}
