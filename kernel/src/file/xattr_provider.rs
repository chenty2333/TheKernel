//! Linux privilege-metadata helpers over the generic inode xattr provider.

use alloc::vec::Vec;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{Location, Metadata, NodePermission, NodeType, XattrSetMode};
use linux_raw_sys::general::{CAP_FOWNER, CAP_SETFCAP, CAP_SYS_ADMIN, R_OK, W_OK};

use super::{
    executable,
    permission::{VfsSecurityContext, check_inode_permissions_with_security, check_writable_mount},
};
use crate::task::{
    FileCapabilities, Kuid, SECURITY_CAPABILITY_XATTR_NAME, ns_capable, parse_file_capabilities,
    security::{
        InodeSecurityRef, InodeXattrOperation, InodeXattrSecurityContext, XattrSetFlags,
        dispatch_inode_xattr,
    },
};

const SECURITY_CAPABILITY_XATTR: &[u8] = SECURITY_CAPABILITY_XATTR_NAME;
const UNSUPPORTED_ACCESS_CONTROL_XATTRS: [&[u8]; 3] = [
    b"system.posix_acl_access",
    b"system.posix_acl_default",
    b"system.richacl",
];
pub(crate) const XATTR_SIZE_MAX: usize = 65_536;

fn absent_or_unsupported(error: AxError) -> bool {
    matches!(
        LinuxError::from(error),
        LinuxError::ENODATA | LinuxError::EOPNOTSUPP
    )
}

/// Reads executable file capabilities without applying userspace xattr
/// namespace visibility policy.
///
/// A provider without xattr support and an absent capability record both mean
/// that the inode contributes no file capabilities. All other storage errors
/// remain visible to the exec transition.
pub(crate) fn read_security_capability(location: &Location) -> AxResult<Option<Vec<u8>>> {
    match location.get_xattr(SECURITY_CAPABILITY_XATTR) {
        Ok(value) => Ok(Some(value)),
        Err(error) if absent_or_unsupported(error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Removes executable file capabilities for an internal killpriv transition.
///
/// Unsupported providers and an already-absent record are successful no-ops;
/// a real provider mutation failure is returned to the caller.
pub(crate) fn remove_security_capability_if_present(location: &Location) -> AxResult<()> {
    match location.remove_xattr(SECURITY_CAPABILITY_XATTR) {
        Ok(()) => Ok(()),
        Err(error) if absent_or_unsupported(error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn actor_capable_in_target_namespace(security: &VfsSecurityContext, capability: u32) -> bool {
    ns_capable(
        security.actor(),
        security.filesystem_owner_user_ns(),
        capability,
    )
}

fn unsupported_access_control_xattr(name: &[u8]) -> bool {
    UNSUPPORTED_ACCESS_CONTROL_XATTRS
        .iter()
        .any(|unsupported| name == *unsupported)
}

/// Linux permits user.* on regular files, directories, FIFOs, and sockets.
fn inode_supports_user_xattrs(node_type: NodeType) -> bool {
    matches!(
        node_type,
        NodeType::RegularFile | NodeType::Directory | NodeType::Fifo | NodeType::Socket
    )
}

fn file_capability_write_allowed(has_setfcap: bool, owns_inode: bool, has_fowner: bool) -> bool {
    has_setfcap && (owns_inode || has_fowner)
}

fn credential_can_set_file_capabilities(
    security: &VfsSecurityContext,
    owner: Option<Kuid>,
) -> bool {
    file_capability_write_allowed(
        actor_capable_in_target_namespace(security, CAP_SETFCAP),
        owner == Some(security.actor().ids().fsuid),
        actor_capable_in_target_namespace(security, CAP_FOWNER),
    )
}

fn authorized_file_capability_mutation<T>(
    metadata: &Metadata,
    security: &VfsSecurityContext,
    operation: impl FnOnce() -> AxResult<T>,
) -> AxResult<T> {
    if metadata.node_type != NodeType::RegularFile {
        return Err(LinuxError::EPERM.into());
    }
    if !credential_can_set_file_capabilities(security, Kuid::from_raw(metadata.uid)) {
        return Err(LinuxError::EPERM.into());
    }
    operation()
}

fn check_namespace_access(
    location: &Location,
    metadata: &Metadata,
    name: &[u8],
    write: bool,
    security: &VfsSecurityContext,
) -> AxResult<()> {
    // ACL records cannot be treated as opaque provider data: accepting one
    // would claim mode synchronization, inheritance, and access enforcement
    // that this VFS does not implement. Reject every direct operation and hide
    // any record imported through lower-level filesystem tooling from lists.
    if unsupported_access_control_xattr(name) {
        return Err(LinuxError::EOPNOTSUPP.into());
    }

    let namespace_end = name
        .iter()
        .position(|byte| *byte == b'.')
        .ok_or(LinuxError::EOPNOTSUPP)?;
    let namespace = &name[..namespace_end];
    if namespace == b"trusted" {
        if !actor_capable_in_target_namespace(security, CAP_SYS_ADMIN) {
            return Err(if write {
                LinuxError::EPERM.into()
            } else {
                LinuxError::ENODATA.into()
            });
        }
        return Ok(());
    }
    if namespace == b"system" || namespace == b"security" {
        // Apart from the dedicated file-capability checks below, security.*
        // authorization belongs to the typed security-module dispatch.
        return Ok(());
    }
    if namespace != b"user" {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    if !inode_supports_user_xattrs(metadata.node_type) {
        return Err(if write {
            LinuxError::EPERM.into()
        } else {
            LinuxError::ENODATA.into()
        });
    }
    if write
        && metadata.node_type == NodeType::Directory
        && metadata.mode.contains(NodePermission::STICKY)
        && Kuid::from_raw(metadata.uid) != Some(security.actor().ids().fsuid)
        && !actor_capable_in_target_namespace(security, CAP_FOWNER)
    {
        return Err(LinuxError::EPERM.into());
    }
    check_inode_permissions_with_security(
        location,
        metadata,
        if write { W_OK } else { R_OK },
        security,
    )
}

fn with_xattr_security<'context, 'location, T>(
    security: &'context VfsSecurityContext,
    location: &'location Location,
    metadata: &Metadata,
    operation: InodeXattrOperation<'context>,
    provider: impl FnOnce() -> AxResult<T>,
) -> AxResult<T> {
    let target = InodeSecurityRef::new(location, metadata);
    let admission = dispatch_inode_xattr(InodeXattrSecurityContext::new(
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
        target,
        operation,
    ))?;
    let result = provider()?;
    admission.committed();
    Ok(result)
}

fn xattr_set_mode(flags: XattrSetFlags) -> XattrSetMode {
    if flags == XattrSetFlags::CREATE {
        XattrSetMode::Create
    } else if flags == XattrSetFlags::REPLACE {
        XattrSetMode::Replace
    } else {
        XattrSetMode::Upsert
    }
}

pub(crate) fn set_xattr_with_security(
    security: &VfsSecurityContext,
    location: &Location,
    name: &[u8],
    value: &[u8],
    flags: XattrSetFlags,
) -> AxResult<()> {
    let transaction = || {
        check_writable_mount(location)?;
        let metadata = location.metadata()?;
        check_namespace_access(location, &metadata, name, true, security)?;
        if name == SECURITY_CAPABILITY_XATTR {
            authorized_file_capability_mutation(&metadata, security, || {
                parse_file_capabilities(value).map(|_| ())
            })?;
        }
        let operation =
            InodeXattrOperation::set(name, value, flags).ok_or(AxError::InvalidInput)?;
        with_xattr_security(security, location, &metadata, operation, || {
            location.set_xattr(name, value, xattr_set_mode(flags))
        })
    };

    if name == SECURITY_CAPABILITY_XATTR {
        executable::with_file_capability_metadata_unpinned(location, transaction)
    } else {
        transaction()
    }
}

pub(crate) fn get_xattr_with_security(
    security: &VfsSecurityContext,
    location: &Location,
    name: &[u8],
) -> AxResult<Vec<u8>> {
    let metadata = location.metadata()?;
    check_namespace_access(location, &metadata, name, false, security)?;
    let operation = InodeXattrOperation::get(name).ok_or(AxError::InvalidInput)?;
    with_xattr_security(security, location, &metadata, operation, || {
        let value = location.get_xattr(name)?;
        if value.len() > XATTR_SIZE_MAX {
            return Err(LinuxError::E2BIG.into());
        }
        Ok(value)
    })
}

fn list_name_visible(metadata: &Metadata, name: &[u8], can_access_trusted: bool) -> bool {
    if unsupported_access_control_xattr(name) {
        return false;
    }
    if name.starts_with(b"trusted.") && !can_access_trusted {
        return false;
    }
    !name.starts_with(b"user.") || inode_supports_user_xattrs(metadata.node_type)
}

fn filter_xattr_list(
    metadata: &Metadata,
    list: &[u8],
    can_access_trusted: bool,
) -> AxResult<Vec<u8>> {
    if list.len() > XATTR_SIZE_MAX {
        return Err(LinuxError::E2BIG.into());
    }
    let mut filtered = Vec::new();
    filtered
        .try_reserve_exact(list.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut remaining = list;
    while !remaining.is_empty() {
        let end = remaining
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(AxError::Io)?;
        if end == 0 {
            return Err(AxError::Io);
        }
        let name = &remaining[..end];
        if list_name_visible(metadata, name, can_access_trusted) {
            filtered.extend_from_slice(name);
            filtered.push(0);
        }
        remaining = &remaining[end + 1..];
    }
    Ok(filtered)
}

pub(crate) fn list_xattrs_with_security(
    security: &VfsSecurityContext,
    location: &Location,
) -> AxResult<Vec<u8>> {
    let metadata = location.metadata()?;
    with_xattr_security(
        security,
        location,
        &metadata,
        InodeXattrOperation::list(),
        || {
            let list = location.list_xattrs()?;
            filter_xattr_list(
                &metadata,
                &list,
                actor_capable_in_target_namespace(security, CAP_SYS_ADMIN),
            )
        },
    )
}

pub(crate) fn remove_xattr_with_security(
    security: &VfsSecurityContext,
    location: &Location,
    name: &[u8],
) -> AxResult<()> {
    let transaction = || {
        check_writable_mount(location)?;
        let metadata = location.metadata()?;
        check_namespace_access(location, &metadata, name, true, security)?;
        if name == SECURITY_CAPABILITY_XATTR {
            authorized_file_capability_mutation(&metadata, security, || Ok(()))?;
        }
        let operation = InodeXattrOperation::remove(name).ok_or(AxError::InvalidInput)?;
        with_xattr_security(security, location, &metadata, operation, || {
            location.remove_xattr(name)
        })
    };

    if name == SECURITY_CAPABILITY_XATTR {
        executable::with_file_capability_metadata_unpinned(location, transaction)
    } else {
        transaction()
    }
}

pub(crate) fn security_capabilities_for_exec(
    location: &Location,
) -> AxResult<Option<FileCapabilities>> {
    let value = read_security_capability(location)?;
    match value.as_deref() {
        Some(value) => parse_file_capabilities(value).map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};

    use axfs_ng_vfs::{Mountpoint, NodePermission, NodeType, XattrSetMode};

    use super::*;
    use crate::{
        pseudofs::tmp::MemoryFs,
        task::{Cred, Kgid, UserNamespace},
    };

    fn valid_v2_capability() -> Vec<u8> {
        vec![
            0x01, 0x00, 0x00, 0x02, // revision 2, effective
            0x01, 0x00, 0x00, 0x00, // permitted word 0
            0x00, 0x00, 0x00, 0x00, // inheritable word 0
            0x00, 0x00, 0x00, 0x00, // permitted word 1
            0x00, 0x00, 0x00, 0x00, // inheritable word 1
        ]
    }

    fn memory_node(node_type: NodeType) -> Location {
        let filesystem = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        mount
            .root_location()
            .create(
                "capability-target",
                node_type,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap()
    }

    fn initial_root() -> Arc<Cred> {
        executable::init().unwrap();
        Cred::try_root(UserNamespace::try_new_root().unwrap()).unwrap()
    }

    #[test]
    fn capability_helpers_share_provider_storage_and_ignore_absence() {
        let filesystem = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        let file = mount
            .root_location()
            .create(
                "capability-provider",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();

        assert_eq!(read_security_capability(&file).unwrap(), None);
        assert_eq!(remove_security_capability_if_present(&file), Ok(()));
        file.set_xattr(
            SECURITY_CAPABILITY_XATTR,
            b"capability-record",
            XattrSetMode::Upsert,
        )
        .unwrap();
        assert_eq!(
            read_security_capability(&file).unwrap(),
            Some(b"capability-record".to_vec())
        );
        remove_security_capability_if_present(&file).unwrap();
        assert_eq!(read_security_capability(&file).unwrap(), None);
    }

    #[test]
    fn capability_absence_classifier_is_narrow() {
        assert!(absent_or_unsupported(LinuxError::ENODATA.into()));
        assert!(absent_or_unsupported(LinuxError::EOPNOTSUPP.into()));
        assert!(!absent_or_unsupported(LinuxError::EIO.into()));
    }

    #[test]
    fn file_capability_authority_requires_every_independent_gate() {
        assert!(file_capability_write_allowed(true, true, false));
        assert!(file_capability_write_allowed(true, false, true));
        assert!(!file_capability_write_allowed(false, true, true));
        assert!(!file_capability_write_allowed(true, false, false));
    }

    #[test]
    fn non_capability_security_writes_reach_typed_security_modules() {
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(root_namespace.clone()).unwrap();
        let child_namespace = root_namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, true)
            .unwrap();
        let security =
            VfsSecurityContext::new(Cred::try_with_user_namespace(&root, child_namespace).unwrap());
        assert!(!actor_capable_in_target_namespace(&security, CAP_SYS_ADMIN));

        let file = memory_node(NodeType::RegularFile);
        set_xattr_with_security(
            &security,
            &file,
            b"security.test",
            b"module-owned",
            XattrSetFlags::NONE,
        )
        .unwrap();
        assert_eq!(file.get_xattr(b"security.test").unwrap(), b"module-owned");
    }

    #[test]
    fn non_utf8_names_round_trip_and_do_not_alias_file_capabilities() {
        let security = VfsSecurityContext::new(initial_root());
        let file = memory_node(NodeType::RegularFile);
        let user_name = b"user.\xff";

        set_xattr_with_security(
            &security,
            &file,
            user_name,
            b"raw-name",
            XattrSetFlags::NONE,
        )
        .unwrap();
        assert_eq!(
            get_xattr_with_security(&security, &file, user_name).unwrap(),
            b"raw-name"
        );
        assert!(
            list_xattrs_with_security(&security, &file)
                .unwrap()
                .split(|byte| *byte == 0)
                .any(|name| name == user_name)
        );

        let near_capability_name = b"security.capabilit\xff";
        set_xattr_with_security(
            &security,
            &file,
            near_capability_name,
            b"opaque",
            XattrSetFlags::NONE,
        )
        .unwrap();
        assert_eq!(
            get_xattr_with_security(&security, &file, near_capability_name).unwrap(),
            b"opaque"
        );
        assert_eq!(read_security_capability(&file).unwrap(), None);

        remove_xattr_with_security(&security, &file, user_name).unwrap();
        remove_xattr_with_security(&security, &file, near_capability_name).unwrap();
    }

    #[test]
    fn child_user_namespace_setfcap_is_not_host_filesystem_authority() {
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(root_namespace.clone()).unwrap();
        let root_security = VfsSecurityContext::new(root.clone());
        assert!(credential_can_set_file_capabilities(
            &root_security,
            Some(Kuid::INITIAL_ROOT)
        ));

        let child_namespace = root_namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, true)
            .unwrap();
        let child = Cred::try_with_user_namespace(&root, child_namespace).unwrap();
        let child_security = VfsSecurityContext::new(child);
        assert!(!credential_can_set_file_capabilities(
            &child_security,
            Some(Kuid::INITIAL_ROOT)
        ));
    }

    #[test]
    fn capability_mutation_rejects_non_regular_and_malformed_targets() {
        let security = VfsSecurityContext::new(initial_root());
        let directory = memory_node(NodeType::Directory);
        assert_eq!(
            set_xattr_with_security(
                &security,
                &directory,
                SECURITY_CAPABILITY_XATTR,
                &valid_v2_capability(),
                XattrSetFlags::NONE,
            ),
            Err(LinuxError::EPERM.into())
        );
        directory
            .set_xattr(
                SECURITY_CAPABILITY_XATTR,
                &valid_v2_capability(),
                XattrSetMode::Upsert,
            )
            .unwrap();
        assert_eq!(
            remove_xattr_with_security(&security, &directory, SECURITY_CAPABILITY_XATTR),
            Err(LinuxError::EPERM.into())
        );
        assert!(directory.get_xattr(SECURITY_CAPABILITY_XATTR).is_ok());

        let file = memory_node(NodeType::RegularFile);
        assert_eq!(
            set_xattr_with_security(
                &security,
                &file,
                SECURITY_CAPABILITY_XATTR,
                &[1, 2, 3],
                XattrSetFlags::NONE,
            ),
            Err(AxError::InvalidInput)
        );
        assert_eq!(read_security_capability(&file).unwrap(), None);
    }

    #[test]
    fn failed_capability_mutations_preserve_the_provider_record() {
        let security = VfsSecurityContext::new(initial_root());
        let file = memory_node(NodeType::RegularFile);
        assert_eq!(
            remove_xattr_with_security(&security, &file, SECURITY_CAPABILITY_XATTR),
            Err(LinuxError::ENODATA.into())
        );

        let capability = valid_v2_capability();
        set_xattr_with_security(
            &security,
            &file,
            SECURITY_CAPABILITY_XATTR,
            &capability,
            XattrSetFlags::NONE,
        )
        .unwrap();
        assert_eq!(
            set_xattr_with_security(
                &security,
                &file,
                SECURITY_CAPABILITY_XATTR,
                &capability,
                XattrSetFlags::CREATE,
            ),
            Err(LinuxError::EEXIST.into())
        );
        assert_eq!(
            file.get_xattr(SECURITY_CAPABILITY_XATTR).unwrap(),
            capability
        );
        remove_xattr_with_security(&security, &file, SECURITY_CAPABILITY_XATTR).unwrap();
        assert_eq!(read_security_capability(&file).unwrap(), None);
    }

    #[test]
    fn exec_reader_distinguishes_absent_and_malformed_provider_values() {
        let file = memory_node(NodeType::RegularFile);
        assert!(security_capabilities_for_exec(&file).unwrap().is_none());
        file.set_xattr(
            SECURITY_CAPABILITY_XATTR,
            &valid_v2_capability(),
            XattrSetMode::Upsert,
        )
        .unwrap();
        assert!(security_capabilities_for_exec(&file).unwrap().is_some());
        file.set_xattr(SECURITY_CAPABILITY_XATTR, &[1, 2, 3], XattrSetMode::Replace)
            .unwrap();
        assert_eq!(
            security_capabilities_for_exec(&file),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn list_filter_preserves_byte_records_and_rejects_provider_corruption() {
        let metadata = memory_node(NodeType::RegularFile).metadata().unwrap();
        let list = b"user.one\0trusted.hidden\0security.test\0";
        assert_eq!(
            filter_xattr_list(&metadata, list, false).unwrap(),
            b"user.one\0security.test\0"
        );
        assert_eq!(
            filter_xattr_list(&metadata, b"user.one", true),
            Err(AxError::Io)
        );
        assert_eq!(filter_xattr_list(&metadata, b"\0", true), Err(AxError::Io));
    }

    #[test]
    fn unsupported_acl_records_are_rejected_and_hidden_end_to_end() {
        let security = VfsSecurityContext::new(initial_root());
        let file = memory_node(NodeType::RegularFile);

        for name in UNSUPPORTED_ACCESS_CONTROL_XATTRS {
            assert_eq!(
                set_xattr_with_security(
                    &security,
                    &file,
                    name,
                    b"unenforced-acl",
                    XattrSetFlags::NONE,
                ),
                Err(LinuxError::EOPNOTSUPP.into())
            );
            file.set_xattr(name, b"imported-acl", XattrSetMode::Upsert)
                .unwrap();
            assert_eq!(
                get_xattr_with_security(&security, &file, name),
                Err(LinuxError::EOPNOTSUPP.into())
            );
            assert_eq!(
                remove_xattr_with_security(&security, &file, name),
                Err(LinuxError::EOPNOTSUPP.into())
            );
        }

        assert!(
            list_xattrs_with_security(&security, &file)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn user_namespace_xattrs_include_fifo_and_socket_inodes() {
        let security = VfsSecurityContext::new(initial_root());
        for node_type in [NodeType::Fifo, NodeType::Socket] {
            let node = memory_node(node_type);
            set_xattr_with_security(
                &security,
                &node,
                b"user.endpoint",
                b"value",
                XattrSetFlags::NONE,
            )
            .unwrap();
            assert_eq!(
                get_xattr_with_security(&security, &node, b"user.endpoint").unwrap(),
                b"value"
            );
            assert_eq!(
                list_xattrs_with_security(&security, &node).unwrap(),
                b"user.endpoint\0"
            );
            remove_xattr_with_security(&security, &node, b"user.endpoint").unwrap();
        }
    }

    #[test]
    fn unsupported_namespace_is_rejected_before_provider_publication() {
        let security = VfsSecurityContext::new(initial_root());
        let file = memory_node(NodeType::RegularFile);
        assert_eq!(
            set_xattr_with_security(
                &security,
                &file,
                b"unknown.value",
                b"value",
                XattrSetFlags::NONE,
            ),
            Err(LinuxError::EOPNOTSUPP.into())
        );
        assert_eq!(
            file.get_xattr(b"unknown.value"),
            Err(LinuxError::ENODATA.into())
        );
    }
}
