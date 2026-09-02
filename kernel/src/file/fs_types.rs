//! The filesystem types constructed by this kernel image.
//!
//! This kernel image has no filesystem module loading or unloading, so
//! `sysfs(2)` can enumerate an immutable build-time catalog.

/// Filesystem type names accepted by the compiled filesystem constructors.
///
/// Entries retain their trailing NUL because `sysfs(2)` option 2 copies that
/// exact representation to userspace.  Keep the order stable: it is part of
/// the syscall's observable ABI.
const FILESYSTEM_TYPE_CATALOG: [&[u8]; 20] = [
    b"ext4\0",
    b"vfat\0",
    b"fat\0",
    b"msdos\0",
    b"tmpfs\0",
    b"hugetlbfs\0",
    b"bpf\0",
    b"proc\0",
    b"sysfs\0",
    b"cgroup\0",
    b"cgroup2\0",
    b"tracefs\0",
    b"debugfs\0",
    b"mqueue\0",
    b"rpc_pipefs\0",
    b"btrfs\0",
    b"xfs\0",
    b"fuse\0",
    b"nfs4\0",
    b"overlay\0",
];

/// Returns the immutable, ordered filesystem type catalog.
pub(crate) fn filesystem_type_catalog() -> &'static [&'static [u8]] {
    &FILESYSTEM_TYPE_CATALOG
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::filesystem_type_catalog;

    #[test]
    fn catalog_is_nul_terminated_unique_and_matches_compiled_support() {
        let catalog = filesystem_type_catalog();
        assert_eq!(
            catalog,
            [
                b"ext4\0".as_slice(),
                b"vfat\0".as_slice(),
                b"fat\0".as_slice(),
                b"msdos\0".as_slice(),
                b"tmpfs\0".as_slice(),
                b"hugetlbfs\0".as_slice(),
                b"bpf\0".as_slice(),
                b"proc\0".as_slice(),
                b"sysfs\0".as_slice(),
                b"cgroup\0".as_slice(),
                b"cgroup2\0".as_slice(),
                b"tracefs\0".as_slice(),
                b"debugfs\0".as_slice(),
                b"mqueue\0".as_slice(),
                b"rpc_pipefs\0".as_slice(),
                b"btrfs\0".as_slice(),
                b"xfs\0".as_slice(),
                b"fuse\0".as_slice(),
                b"nfs4\0".as_slice(),
                b"overlay\0".as_slice(),
            ]
        );

        let mut names = Vec::new();
        for name in catalog {
            assert!(name.len() > 1);
            assert_eq!(name.last(), Some(&0));
            assert!(!name[..name.len() - 1].contains(&0));
            assert!(!names.contains(name));
            names.push(*name);
        }
    }
}
