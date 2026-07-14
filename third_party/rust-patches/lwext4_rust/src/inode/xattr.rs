use alloc::vec::Vec;
use core::{ffi::c_char, ptr};

use super::InodeRef;
use crate::{
    Ext4Error, Ext4Result, SystemHal,
    error::Context,
    ffi::{
        EINVAL, ENODATA, ENOMEM, ERANGE, ext4_extract_xattr_name, ext4_xattr_get,
        ext4_xattr_list_names, ext4_xattr_remove, ext4_xattr_set_status,
    },
};

#[derive(Debug)]
struct ParsedXattrName {
    index: u8,
    name: *const c_char,
    len: usize,
}

fn parse_xattr_name(name: &[u8]) -> Ext4Result<ParsedXattrName> {
    if name.contains(&0) {
        return Err(Ext4Error::new(EINVAL as _, "xattr name contains NUL"));
    }

    let mut index = 0;
    let mut len = 0;
    let mut found = false;
    let parsed = unsafe {
        ext4_extract_xattr_name(
            name.as_ptr().cast(),
            name.len(),
            &mut index,
            &mut len,
            &mut found,
        )
    };
    if !found {
        return Err(Ext4Error::new(
            EINVAL as _,
            "unsupported ext4 xattr namespace",
        ));
    }
    if len > u8::MAX as usize {
        return Err(Ext4Error::new(ERANGE as _, "ext4 xattr name is too long"));
    }
    if len != 0 && parsed.is_null() {
        return Err(Ext4Error::new(EINVAL as _, "invalid ext4 xattr name"));
    }
    Ok(ParsedXattrName {
        index,
        name: parsed,
        len,
    })
}

fn try_zeroed(len: usize) -> Ext4Result<Vec<u8>> {
    let mut value = Vec::new();
    value
        .try_reserve_exact(len)
        .map_err(|_| Ext4Error::new(ENOMEM as _, "xattr snapshot allocation failed"))?;
    value.resize(len, 0);
    Ok(value)
}

impl<Hal: SystemHal> InodeRef<Hal> {
    fn xattr_size(&self, name: &[u8]) -> Ext4Result<usize> {
        let name = parse_xattr_name(name)?;
        let mut required = 0;
        unsafe {
            ext4_xattr_get(
                self.inner.as_ref() as *const _ as *mut _,
                name.index,
                name.name,
                name.len,
                ptr::null_mut(),
                0,
                &mut required,
            )
        }
        .context("ext4_xattr_get size")?;
        Ok(required)
    }

    pub fn has_xattr(&self, name: &[u8]) -> Ext4Result<bool> {
        match self.xattr_size(name) {
            Ok(_) => Ok(true),
            Err(error) if error.code == ENODATA as i32 => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn get_xattr(&self, name: &[u8]) -> Ext4Result<Vec<u8>> {
        let name = parse_xattr_name(name)?;
        let mut required = 0;
        unsafe {
            ext4_xattr_get(
                self.inner.as_ref() as *const _ as *mut _,
                name.index,
                name.name,
                name.len,
                ptr::null_mut(),
                0,
                &mut required,
            )
        }
        .context("ext4_xattr_get size")?;

        let mut value = try_zeroed(required)?;
        let mut actual = 0;
        unsafe {
            ext4_xattr_get(
                self.inner.as_ref() as *const _ as *mut _,
                name.index,
                name.name,
                name.len,
                value.as_mut_ptr().cast(),
                value.len(),
                &mut actual,
            )
        }
        .context("ext4_xattr_get snapshot")?;
        if actual > value.len() {
            return Err(Ext4Error::new(
                ERANGE as _,
                "ext4 xattr grew during an atomic snapshot",
            ));
        }
        value.truncate(actual);
        Ok(value)
    }

    pub fn list_xattrs(&self) -> Ext4Result<Vec<u8>> {
        let mut required = 0;
        unsafe {
            ext4_xattr_list_names(
                self.inner.as_ref() as *const _ as *mut _,
                ptr::null_mut(),
                0,
                &mut required,
            )
        }
        .context("ext4_xattr_list_names size")?;

        let mut names = try_zeroed(required)?;
        let mut actual = 0;
        unsafe {
            ext4_xattr_list_names(
                self.inner.as_ref() as *const _ as *mut _,
                names.as_mut_ptr().cast(),
                names.len(),
                &mut actual,
            )
        }
        .context("ext4_xattr_list_names snapshot")?;
        if actual > names.len() {
            return Err(Ext4Error::new(
                ERANGE as _,
                "ext4 xattr list grew during an atomic snapshot",
            ));
        }
        names.truncate(actual);
        Ok(names)
    }

    pub fn set_xattr(&mut self, name: &[u8], value: &[u8]) -> Ext4Result<()> {
        let name = parse_xattr_name(name)?;
        let mut metadata_may_have_changed = false;
        unsafe {
            ext4_xattr_set_status(
                self.inner.as_mut(),
                name.index,
                name.name,
                name.len,
                value.as_ptr().cast(),
                value.len(),
                &mut metadata_may_have_changed,
            )
        }
        .context("ext4_xattr_set")
        .map_err(|error| error.with_metadata_may_have_changed(metadata_may_have_changed))
    }

    pub fn remove_xattr(&mut self, name: &[u8]) -> Ext4Result<()> {
        let name = parse_xattr_name(name)?;
        unsafe { ext4_xattr_remove(self.inner.as_mut(), name.index, name.name, name.len) }
            .context("ext4_xattr_remove")
            .map_err(|error| {
                let changed = error.code != ENODATA as i32;
                error.with_metadata_may_have_changed(changed)
            })
    }
}

#[cfg(test)]
mod tests {
    use alloc::{format, string::String, vec};
    use core::slice;
    use std::{
        fs::{self, OpenOptions},
        process::Command,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::{
        BlockDevice, DummyHal, Ext4Filesystem, FsConfig, InodeType,
        blockdev::EXT4_DEV_BSIZE,
        ffi::{EIO, ENOSPC, EXT4_ROOT_INO},
    };

    #[derive(Clone)]
    struct SharedImage(Arc<Mutex<Vec<u8>>>);

    impl SharedImage {
        fn range(&self, block_id: u64, len: usize) -> Ext4Result<core::ops::Range<usize>> {
            let start = usize::try_from(block_id)
                .ok()
                .and_then(|block| block.checked_mul(EXT4_DEV_BSIZE))
                .ok_or_else(|| Ext4Error::new(EIO as _, "test image offset overflow"))?;
            let end = start
                .checked_add(len)
                .ok_or_else(|| Ext4Error::new(EIO as _, "test image range overflow"))?;
            if end > self.0.lock().unwrap().len() {
                return Err(Ext4Error::new(EIO as _, "test image access out of range"));
            }
            Ok(start..end)
        }
    }

    impl BlockDevice for SharedImage {
        fn write_blocks(&mut self, block_id: u64, buf: &[u8]) -> Ext4Result<usize> {
            let range = self.range(block_id, buf.len())?;
            self.0.lock().unwrap()[range].copy_from_slice(buf);
            Ok(buf.len())
        }

        fn read_blocks(&mut self, block_id: u64, buf: &mut [u8]) -> Ext4Result<usize> {
            let range = self.range(block_id, buf.len())?;
            buf.copy_from_slice(&self.0.lock().unwrap()[range]);
            Ok(buf.len())
        }

        fn num_blocks(&self) -> Ext4Result<u64> {
            Ok((self.0.lock().unwrap().len() / EXT4_DEV_BSIZE) as u64)
        }
    }

    fn formatted_ext4_image() -> SharedImage {
        let path = std::env::temp_dir().join(format!(
            "lwext4-xattr-{}-{}.img",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len(16 * 1024 * 1024).unwrap();
        drop(file);
        let status = Command::new("mke2fs")
            .args([
                "-q",
                "-t",
                "ext4",
                "-F",
                "-b",
                "4096",
                "-O",
                "none,has_journal,ext_attr,dir_index,filetype,extent,64bit,flex_bg,sparse_super,\
                 large_file,huge_file,dir_nlink,extra_isize,metadata_csum",
            ])
            .arg(&path)
            .status()
            .expect("mke2fs is required for the lwext4 persistence test");
        assert!(status.success());
        let image = fs::read(&path).unwrap();
        fs::remove_file(path).unwrap();
        SharedImage(Arc::new(Mutex::new(image)))
    }

    fn open_test_fs(image: &SharedImage) -> Ext4Filesystem<DummyHal, SharedImage> {
        Ext4Filesystem::new(image.clone(), FsConfig::default()).unwrap()
    }

    fn shared_external_xattr_image() -> SharedImage {
        let image = formatted_ext4_image();
        let stem = format!(
            "lwext4-xattr-cow-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        );
        let path = std::env::temp_dir().join(format!("{stem}.img"));
        let source = std::env::temp_dir().join(format!("{stem}.source"));
        let value = std::env::temp_dir().join(format!("{stem}.value"));
        fs::write(&path, &*image.0.lock().unwrap()).unwrap();
        fs::write(&source, b"x").unwrap();
        fs::write(&value, vec![0; 1024]).unwrap();

        for command in [
            format!("write {} /a", source.display()),
            format!("write {} /b", source.display()),
            format!("ea_set -f {} /a user.shared", value.display()),
            format!("ea_set -f {} /b user.shared", value.display()),
        ] {
            let status = Command::new("debugfs")
                .args(["-w", "-R", &command])
                .arg(&path)
                .status()
                .expect("debugfs is required for the shared-xattr COW test");
            assert!(status.success());
        }

        let stat = Command::new("debugfs")
            .args(["-R", "stat /a"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(stat.status.success());
        let stat = String::from_utf8(stat.stdout).unwrap();
        let block = stat
            .lines()
            .find_map(|line| line.strip_prefix("File ACL: "))
            .expect("debugfs did not report an external xattr block");
        let command = format!("set_inode_field /b file_acl {block}");
        let status = Command::new("debugfs")
            .args(["-w", "-R", &command])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        // Normalize the handcrafted shared reference before the strict final fsck.
        let output = Command::new("e2fsck")
            .arg("-fy")
            .arg(&path)
            .output()
            .expect("e2fsck is required for the shared-xattr COW test");
        assert!(
            matches!(output.status.code(), Some(0) | Some(1)),
            "e2fsck could not normalize the shared-xattr fixture:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let image = SharedImage(Arc::new(Mutex::new(fs::read(&path).unwrap())));
        for path in [&path, &source, &value] {
            fs::remove_file(path).unwrap();
        }
        image
    }

    fn assert_clean_ext4_image(image: &SharedImage) {
        let path = std::env::temp_dir().join(format!(
            "lwext4-xattr-fsck-{}-{}.img",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::write(&path, &*image.0.lock().unwrap()).unwrap();
        let output = Command::new("e2fsck")
            .args(["-fn"])
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(path).unwrap();
        assert!(
            output.status.success(),
            "e2fsck rejected the mutated image:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn suffix<'a>(parsed: &ParsedXattrName, source: &'a [u8]) -> &'a [u8] {
        if parsed.len == 0 {
            return &[];
        }
        let bytes = unsafe { slice::from_raw_parts(parsed.name.cast::<u8>(), parsed.len) };
        let offset = bytes.as_ptr() as usize - source.as_ptr() as usize;
        &source[offset..offset + parsed.len]
    }

    #[test]
    fn parser_preserves_ext4_namespace_indexes_and_suffixes() {
        let user = b"user.key";
        let parsed = parse_xattr_name(user).unwrap();
        assert_eq!(parsed.index, 1);
        assert_eq!(suffix(&parsed, user), b"key");

        let acl = b"system.posix_acl_access";
        let parsed = parse_xattr_name(acl).unwrap();
        assert_eq!(parsed.index, 2);
        assert_eq!(suffix(&parsed, acl), b"");

        let richacl = b"system.richacl";
        let parsed = parse_xattr_name(richacl).unwrap();
        assert_eq!(parsed.index, 8);
        assert_eq!(suffix(&parsed, richacl), b"");

        assert_eq!(parse_xattr_name(b"user.").unwrap_err().code, EINVAL as i32);
        assert_eq!(
            parse_xattr_name(b"unknown.value").unwrap_err().code,
            EINVAL as i32
        );
    }

    #[test]
    fn vfs_style_preflight_allows_first_xattr_on_a_fresh_inode() {
        let image = formatted_ext4_image();
        let mut fs = open_test_fs(&image);
        let (token, _) = fs
            .create(
                EXT4_ROOT_INO,
                "xattr-first-set",
                InodeType::RegularFile,
                0o600,
                None,
                None,
                None,
            )
            .unwrap();
        let ino = token.ino();

        fs.with_inode_ref_mut(ino, |inode| {
            assert!(!inode.has_xattr(b"user.first")?);
            inode.set_xattr(b"user.first", b"published")?;
            assert_eq!(inode.get_xattr(b"user.first")?, b"published");
            Ok(())
        })
        .unwrap();

        fs.release_inode_handle(token);
        fs.shutdown().unwrap();
    }

    #[test]
    fn raw_xattr_names_survive_real_ext4_reopen_and_remove() {
        let image = formatted_ext4_image();
        let raw_name = b"user.raw-\xff-name";
        let mut boundary_name = b"user.".to_vec();
        boundary_name.resize(255, 0xfe);
        assert_eq!(boundary_name.len(), 255);

        let mut fs = open_test_fs(&image);
        let (token, _) = fs
            .create(
                EXT4_ROOT_INO,
                "raw-xattr-names",
                InodeType::RegularFile,
                0o600,
                None,
                None,
                None,
            )
            .unwrap();
        let ino = token.ino();
        fs.with_inode_ref_mut(ino, |inode| {
            inode.set_xattr(raw_name, b"raw")?;
            inode.set_xattr(&boundary_name, b"boundary")?;
            assert_eq!(inode.get_xattr(raw_name)?, b"raw");
            assert_eq!(inode.get_xattr(&boundary_name)?, b"boundary");
            Ok(())
        })
        .unwrap();
        fs.release_inode_handle(token);
        fs.shutdown().unwrap();
        drop(fs);

        let mut fs = open_test_fs(&image);
        let (ino, _) = fs.lookup_inode(EXT4_ROOT_INO, "raw-xattr-names").unwrap();
        fs.with_inode_ref_mut(ino, |inode| {
            assert_eq!(inode.get_xattr(raw_name)?, b"raw");
            assert_eq!(inode.get_xattr(&boundary_name)?, b"boundary");
            let listed = inode.list_xattrs()?;
            let names = listed
                .split(|byte| *byte == 0)
                .filter(|name| !name.is_empty())
                .collect::<Vec<_>>();
            assert!(names.contains(&raw_name.as_slice()));
            assert!(names.contains(&boundary_name.as_slice()));

            inode.remove_xattr(raw_name)?;
            inode.remove_xattr(&boundary_name)?;
            assert!(!inode.has_xattr(raw_name)?);
            assert!(!inode.has_xattr(&boundary_name)?);
            assert!(inode.list_xattrs()?.is_empty());
            Ok(())
        })
        .unwrap();
        fs.shutdown().unwrap();
        drop(fs);

        let mut fs = open_test_fs(&image);
        let (ino, _) = fs.lookup_inode(EXT4_ROOT_INO, "raw-xattr-names").unwrap();
        fs.with_inode_ref(ino, |inode| {
            assert!(!inode.has_xattr(raw_name)?);
            assert!(!inode.has_xattr(&boundary_name)?);
            assert!(inode.list_xattrs()?.is_empty());
            Ok(())
        })
        .unwrap();
        fs.shutdown().unwrap();
        drop(fs);

        assert_clean_ext4_image(&image);
    }

    #[test]
    fn clean_xattr_enospc_preserves_the_old_value_and_filesystem() {
        let image = formatted_ext4_image();
        let mut fs = open_test_fs(&image);
        let (token, _) = fs
            .create(
                EXT4_ROOT_INO,
                "xattr-clean-enospc",
                InodeType::RegularFile,
                0o600,
                None,
                None,
                None,
            )
            .unwrap();
        let ino = token.ino();
        fs.with_inode_ref_mut(ino, |inode| inode.set_xattr(b"user.keep", b"old"))
            .unwrap();

        let oversized = vec![0x5a; 8 * 1024];
        let error = fs
            .with_inode_ref_mut(ino, |inode| inode.set_xattr(b"user.keep", &oversized))
            .unwrap_err();
        assert_eq!(error.code, ENOSPC as i32);
        assert!(!error.metadata_may_have_changed());

        fs.with_inode_ref_mut(ino, |inode| inode.set_xattr(b"user.after", b"usable"))
            .unwrap();
        fs.with_inode_ref(ino, |inode| {
            assert_eq!(inode.get_xattr(b"user.keep")?, b"old");
            assert_eq!(inode.get_xattr(b"user.after")?, b"usable");
            Ok(())
        })
        .unwrap();

        fs.release_inode_handle(token);
        fs.shutdown().unwrap();
    }

    #[test]
    fn c_get_reports_erange_without_truncating_the_value() {
        let image = formatted_ext4_image();
        let mut fs = open_test_fs(&image);
        let (token, _) = fs
            .create(
                EXT4_ROOT_INO,
                "xattr-erange",
                InodeType::RegularFile,
                0o600,
                None,
                None,
                None,
            )
            .unwrap();
        let ino = token.ino();
        fs.with_inode_ref_mut(ino, |inode| inode.set_xattr(b"user.value", b"complete"))
            .unwrap();

        fs.with_inode_ref(ino, |inode| {
            let name = parse_xattr_name(b"user.value")?;
            let mut output = [0xcc; 2];
            let mut required = 0;
            let result = unsafe {
                ext4_xattr_get(
                    inode.inner.as_ref() as *const _ as *mut _,
                    name.index,
                    name.name,
                    name.len,
                    output.as_mut_ptr().cast(),
                    output.len(),
                    &mut required,
                )
            };
            assert_eq!(result, ERANGE as i32);
            assert_eq!(required, b"complete".len());
            assert_eq!(output, [0xcc; 2]);
            Ok(())
        })
        .unwrap();

        fs.release_inode_handle(token);
        fs.shutdown().unwrap();
    }

    #[test]
    fn shared_xattr_cow_preserves_both_block_checksums() {
        let image = shared_external_xattr_image();
        let mut fs = open_test_fs(&image);
        let (first, _) = fs.lookup_inode(EXT4_ROOT_INO, "a").unwrap();
        let (second, _) = fs.lookup_inode(EXT4_ROOT_INO, "b").unwrap();
        fs.with_inode_ref(first, |inode| {
            assert_eq!(inode.get_xattr(b"user.shared")?, vec![0; 1024]);
            Ok(())
        })
        .unwrap();
        fs.with_inode_ref_mut(first, |inode| {
            inode.set_xattr(b"user.shared", &vec![0x5a; 1024])
        })
        .unwrap();
        fs.with_inode_ref(first, |inode| {
            assert_eq!(inode.get_xattr(b"user.shared")?, vec![0x5a; 1024]);
            Ok(())
        })
        .unwrap();
        fs.with_inode_ref(second, |inode| {
            assert_eq!(inode.get_xattr(b"user.shared")?, vec![0; 1024]);
            Ok(())
        })
        .unwrap();
        fs.shutdown().unwrap();
        drop(fs);

        assert_clean_ext4_image(&image);
    }

    #[test]
    fn xattrs_survive_reopen_and_mutate_the_real_ext4_image() {
        let image = formatted_ext4_image();
        let mut fs = open_test_fs(&image);
        let (token, _) = fs
            .create(
                EXT4_ROOT_INO,
                "xattr-persistence",
                InodeType::RegularFile,
                0o600,
                None,
                None,
                None,
            )
            .unwrap();
        let ino = token.ino();
        let first_large = vec![0x5a; 1024];
        fs.with_inode_ref_mut(ino, |inode| {
            inode.set_xattr(b"user.inline", b"disk-backed")?;
            inode.set_xattr(b"user.external", &first_large)
        })
        .unwrap();
        fs.release_inode_handle(token);
        fs.shutdown().unwrap();
        drop(fs);

        let mut fs = open_test_fs(&image);
        let (ino, node_type) = fs.lookup_inode(EXT4_ROOT_INO, "xattr-persistence").unwrap();
        assert_eq!(node_type, InodeType::RegularFile);
        fs.with_inode_ref(ino, |inode| {
            assert_eq!(inode.get_xattr(b"user.inline")?, b"disk-backed");
            assert_eq!(inode.get_xattr(b"user.external")?, first_large);
            let listed = inode.list_xattrs()?;
            let mut names = listed
                .split(|byte| *byte == 0)
                .filter(|name| !name.is_empty())
                .collect::<Vec<_>>();
            names.sort_unstable();
            assert_eq!(names, [&b"user.external"[..], &b"user.inline"[..]]);
            Ok(())
        })
        .unwrap();

        let second_large = vec![0xa5; 1536];
        fs.with_inode_ref_mut(ino, |inode| {
            inode.remove_xattr(b"user.inline")?;
            inode.set_xattr(b"user.external", &second_large)
        })
        .unwrap();
        fs.shutdown().unwrap();
        drop(fs);

        let mut fs = open_test_fs(&image);
        let (ino, _) = fs.lookup_inode(EXT4_ROOT_INO, "xattr-persistence").unwrap();
        fs.with_inode_ref(ino, |inode| {
            assert!(!inode.has_xattr(b"user.inline")?);
            assert_eq!(inode.get_xattr(b"user.external")?, second_large);
            assert_eq!(inode.list_xattrs()?, b"user.external\0");
            Ok(())
        })
        .unwrap();
        fs.shutdown().unwrap();
    }
}
