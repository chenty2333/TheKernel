//! Linux-shaped ACPI thermal-zone attributes; reads never substitute zero.
use alloc::{borrow::Cow, format, sync::Arc, vec::Vec};

use axfs_ng_vfs::{FsName, FsNameBuf, VfsError, VfsResult};

use super::{
    DirMapping, NodeOpsMux, SimpleDir, SimpleDirOps, SimpleFile, SimpleFs,
    dir::{ChildNames, try_boxed_names},
};
struct Zones {
    fs: Arc<SimpleFs>,
}
impl SimpleDirOps for Zones {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let names = crate::acpi::thermal::zones();
        let mut v = Vec::new();
        v.try_reserve_exact(names.len())
            .map_err(|_| VfsError::NoMemory)?;
        for i in 0..names.len() {
            v.push(Cow::Owned(
                FsNameBuf::from_vec(format!("thermal_zone{i}").into_bytes())
                    .map_err(|_| VfsError::InvalidData)?,
            ));
        }
        try_boxed_names(v.into_iter())
    }
    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let n = core::str::from_utf8(name.as_bytes())
            .map_err(|_| VfsError::NotFound)?
            .strip_prefix("thermal_zone")
            .and_then(|n| n.parse::<usize>().ok())
            .ok_or(VfsError::NotFound)?;
        let path = crate::acpi::thermal::zones()
            .get(n)
            .cloned()
            .ok_or(VfsError::NotFound)?;
        let mut dir = DirMapping::new();
        dir.add(
            "type",
            SimpleFile::new_regular(self.fs.clone(), || Ok::<_, VfsError>("acpitz\n")),
        );
        let temp_path = path.clone();
        dir.add(
            "temp",
            SimpleFile::new_regular(self.fs.clone(), move || {
                crate::acpi::thermal::temperature(&temp_path, "_TMP")
                    .map(|t| format!("{t}\n"))
                    .map_err(|_| VfsError::Io)
            }),
        );
        let mut trip = 0;
        for (method, kind) in [("_CRT", "critical"), ("_PSV", "passive")] {
            if crate::acpi::thermal::temperature(&path, method).is_err() {
                continue;
            }
            let path = path.clone();
            dir.add(
                format!("trip_point_{trip}_temp"),
                SimpleFile::new_regular(self.fs.clone(), move || {
                    crate::acpi::thermal::temperature(&path, method)
                        .map(|t| format!("{t}\n"))
                        .map_err(|_| VfsError::Io)
                }),
            );
            dir.add(
                format!("trip_point_{trip}_type"),
                SimpleFile::new_regular(self.fs.clone(), move || {
                    Ok::<_, VfsError>(format!("{kind}\n"))
                }),
            );
            trip += 1;
        }
        Ok(SimpleDir::new_maker(self.fs.clone(), Arc::new(dir)).into())
    }
    fn is_cacheable(&self) -> bool {
        false
    }
}
pub(super) fn class_root(fs: Arc<SimpleFs>) -> DirMapping {
    let mut dir = DirMapping::new();
    dir.add(
        "thermal",
        SimpleDir::new_maker(fs.clone(), Arc::new(Zones { fs })),
    );
    dir
}
