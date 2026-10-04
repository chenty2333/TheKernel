//! Linux-formatted inventories, without inventing loaded modules or providers.
//! Format facts: Linux 7.2.3 fs/filesystems.c and kernel/module/procfs.c.

use alloc::{string::String, sync::Arc};
use core::fmt::Write;

use super::{DirMapping, SimpleFile, SimpleFs};

fn render_filesystems(types: impl Iterator<Item = (&'static str, bool)>) -> String {
    let mut output = String::new();
    for (name, requires_device) in types {
        let _ = writeln!(
            output,
            "{}\t{name}",
            if requires_device { "" } else { "nodev" }
        );
    }
    output
}

pub(super) fn register(root: &mut DirMapping, fs: &Arc<SimpleFs>) {
    root.add(
        "filesystems",
        SimpleFile::new_regular(fs.clone(), || {
            Ok(render_filesystems(
                crate::syscall::fs::proc_filesystem_types(),
            ))
        }),
    );
    // All providers are linked into the image. Linux lists loaded modules,
    // not built-in drivers; the empty set is an empty file with no header.
    root.add("modules", SimpleFile::new_regular(fs.clone(), || Ok("")));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystems_have_linux_nodev_marker_and_tab_separator() {
        assert_eq!(
            render_filesystems([("proc", false), ("ext4", true)].into_iter()),
            "nodev\tproc\n\text4\n"
        );
        assert_eq!(render_filesystems(core::iter::empty()), "");
    }

    #[test]
    fn filesystem_inventory_uses_unique_mount_providers() {
        let providers: alloc::vec::Vec<_> = crate::syscall::fs::proc_filesystem_types().collect();
        for (index, (name, requires_device)) in providers.iter().enumerate() {
            assert!(!providers[..index].iter().any(|(other, _)| other == name));
            assert_eq!(
                *requires_device,
                matches!(*name, "ext4" | "vfat" | "btrfs" | "xfs")
            );
        }
        assert!(providers.contains(&("proc", false)));
        assert!(providers.contains(&("cgroup2", false)));
    }
}
