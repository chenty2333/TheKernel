//! Real backing-inode coverage of userxattr copy-up, private markers and remount.
use axfs::{OverlayFilesystem, OverlayMountOptions, OverlayTopology};
use axfs_ng_vfs::{FsName, Location, Mountpoint, NodePermission, NodeType, VfsError, XattrSetMode};

use super::MemoryFs;

fn child(parent: &Location, name: &[u8], kind: NodeType) -> Location {
    parent
        .create(
            FsName::new(name),
            kind,
            NodePermission::from_bits_truncate(0o777),
        )
        .unwrap()
}

#[test]
fn userxattr_backing_markers_drive_copyup_whiteout_opaque_and_remount() {
    let _context = crate::test_support::scheduler_test_context();
    let backing = MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
    let root = Mountpoint::new_root(&backing).root_location();
    let lower = child(&root, b"lower", NodeType::Directory);
    let upper = child(&root, b"upper", NodeType::Directory);
    let work = child(&root, b"work", NodeType::Directory);
    let original = child(&lower, b"file", NodeType::RegularFile);
    let low_opaque = child(&lower, b"opaque", NodeType::Directory);
    child(&low_opaque, b"hidden", NodeType::RegularFile);
    let up_opaque = child(&upper, b"opaque", NodeType::Directory);
    up_opaque
        .set_xattr(b"user.overlay.opaque", b"y", XattrSetMode::Upsert)
        .unwrap();
    let plain_lower = child(&lower, b"plain", NodeType::Directory);
    child(&plain_lower, b"child", NodeType::RegularFile);
    let plain_upper = child(&upper, b"plain", NodeType::Directory);
    plain_upper.set_xattr(b"user.overlay.redirect", b"forged", XattrSetMode::Upsert).unwrap();
    let ordinary = child(&upper, b"trusted-marker", NodeType::RegularFile);
    ordinary
        .set_xattr(b"trusted.overlay.whiteout", b"y", XattrSetMode::Upsert)
        .unwrap();
    let mut options = OverlayMountOptions::empty();
    options.set_option(b"lowerdir", b"/lower").unwrap();
    options.set_option(b"upperdir", b"/upper").unwrap();
    options.set_option(b"workdir", b"/work").unwrap();
    options.set_flag(b"userxattr").unwrap();
    let topology = OverlayTopology::try_new(
        &options,
        alloc::vec![lower.clone()],
        Some(upper.clone()),
        Some(work),
    )
    .unwrap();
    let overlay = OverlayFilesystem::new(topology.clone()).unwrap();
    let merged = Mountpoint::new_root(&overlay).root_location();
    let found = merged.lookup_no_follow_in_mount(FsName::new(b"trusted-marker"));
    assert!(found.is_ok(), "lookup error: {:?}", found.map(|_| ()));
    let plain = merged.lookup_no_follow_in_mount(FsName::new(b"plain")).unwrap();
    assert!(plain.lookup_no_follow_in_mount(FsName::new(b"child")).is_ok());
    let opaque = merged.lookup_no_follow_in_mount(FsName::new(b"opaque")).unwrap();
    assert!(matches!(
        opaque.lookup_no_follow_in_mount(FsName::new(b"hidden")),
        Err(VfsError::NotFound)
    ));
    let file = merged.lookup_no_follow_in_mount(FsName::new(b"file")).unwrap();
    file.set_xattr(b"user.note", b"copied", XattrSetMode::Upsert)
        .unwrap();
    let copied = upper.lookup_no_follow_in_mount(FsName::new(b"file")).unwrap();
    assert_eq!(copied.get_xattr(b"user.overlay.origin").unwrap().len(), 24);
    assert_eq!(copied.get_xattr(b"trusted.overlay.origin").unwrap_err(), axerrno::LinuxError::ENODATA.into());
    assert_eq!(original.get_xattr(b"user.note").unwrap_err(), axerrno::LinuxError::ENODATA.into());
    let listed = file.list_xattrs().unwrap();
    assert!(
        listed
            .split(|byte| *byte == 0)
            .any(|name| name == b"user.note")
    );
    assert!(
        !listed
            .split(|byte| *byte == 0)
            .any(|name| name.starts_with(b"user.overlay."))
    );
    assert_eq!(
        file.set_xattr(b"user.overlay.origin", b"forged", XattrSetMode::Upsert),
        Err(VfsError::OperationNotSupported)
    );
    merged.unlink(FsName::new(b"file"), false).unwrap();
    let whiteout = upper.lookup_no_follow_in_mount(FsName::new(b"file")).unwrap();
    assert_eq!(whiteout.get_xattr(b"user.overlay.whiteout").unwrap(), b"y");
    assert_eq!(whiteout.get_xattr(b"trusted.overlay.whiteout").unwrap_err(), axerrno::LinuxError::ENODATA.into());
    assert!(matches!(
        merged.lookup_no_follow_in_mount(FsName::new(b"file")),
        Err(VfsError::NotFound)
    ));
    let remounted = OverlayFilesystem::new(topology).unwrap();
    let again = Mountpoint::new_root(&remounted).root_location();
    assert!(matches!(
        again.lookup_no_follow_in_mount(FsName::new(b"file")),
        Err(VfsError::NotFound)
    ));
}
