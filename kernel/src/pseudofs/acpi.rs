//! Root-only table bytes and live ACPI namespace views. No table-byte logging.
use alloc::{borrow::Cow, format, string::String, sync::Arc, vec::Vec};

use axfs_ng_vfs::{FsName, FsNameBuf, NodePermission, VfsError, VfsResult};

use super::{
    DirMaker, DirMapping, NodeOpsMux, SimpleDir, SimpleDirOps, SimpleFile, SimpleFs,
    dir::{ChildNames, try_boxed_names},
};
#[derive(Clone, Copy)]
enum View {
    Tables,
    Devices,
}
struct Directory {
    fs: Arc<SimpleFs>,
    view: View,
}
fn table_names() -> Vec<(String, u32)> {
    crate::acpi::with_engine(|engine| {
        let mut result = Vec::new();
        let mut counts = alloc::collections::BTreeMap::<String, u32>::new();
        for index in 0..engine.table_count().unwrap_or(0) {
            let Ok(bytes) = engine.table(index) else {
                continue;
            };
            if bytes.len() < 8 {
                continue;
            }
            let Ok(signature) = core::str::from_utf8(&bytes[..4]) else {
                continue;
            };
            if !signature
                .bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'!')
            {
                continue;
            }
            let count = counts.entry(String::from(signature)).or_default();
            *count += 1;
            let name = if *count == 1 {
                String::from(signature)
            } else {
                format!("{signature}{count}")
            };
            result.push((name, index));
        }
        result
    })
    .unwrap_or_default()
}
fn device_names() -> Vec<(String, String)> {
    let mut counts = alloc::collections::BTreeMap::<String, u32>::new();
    crate::acpi::with_engine(|e| {
        e.namespace()
            .unwrap_or_default()
            .into_iter()
            .filter(|n| n.kind == 6)
            .map(|n| {
                let hid = e.hardware_id(&n.path).unwrap_or_else(|_| "device".into());
                let count = counts.entry(hid.clone()).or_default();
                let name = format!("{hid}:{count:02x}");
                *count += 1;
                (name, n.path)
            })
            .collect()
    })
    .unwrap_or_default()
}
impl SimpleDirOps for Directory {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let names = match self.view {
            View::Tables => core::iter::once(String::from("dynamic"))
                .chain(table_names().into_iter().map(|p| p.0))
                .collect::<Vec<_>>(),
            View::Devices => device_names().into_iter().map(|p| p.0).collect(),
        };
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(names.len())
            .map_err(|_| VfsError::NoMemory)?;
        for name in names {
            owned.push(Cow::Owned(
                FsNameBuf::from_vec(name.into_bytes()).map_err(|_| VfsError::InvalidData)?,
            ));
        }
        try_boxed_names(owned.into_iter())
    }
    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        match self.view {
            View::Tables => {
                if name.as_bytes() == b"dynamic" {
                    return Ok(
                        SimpleDir::new_maker(self.fs.clone(), Arc::new(DirMapping::new())).into(),
                    );
                }
                let (_, index) = table_names()
                    .into_iter()
                    .find(|(n, _)| n.as_bytes() == name.as_bytes())
                    .ok_or(VfsError::NotFound)?;
                Ok(SimpleFile::new_regular_with_permission(
                    self.fs.clone(),
                    NodePermission::from_bits_truncate(0o400),
                    move || {
                        crate::acpi::with_engine(|e| e.table(index).map_err(|_| VfsError::Io))
                            .unwrap_or(Err(VfsError::NotFound))
                    },
                )
                .into())
            }
            View::Devices => {
                let (_, path) = device_names()
                    .into_iter()
                    .find(|(n, _)| n.as_bytes() == name.as_bytes())
                    .ok_or(VfsError::NotFound)?;
                let mut dir = DirMapping::new();
                let hidpath = path.clone();
                dir.add(
                    "path",
                    SimpleFile::new_regular(self.fs.clone(), {
                        let path = path.clone();
                        move || Ok::<_, VfsError>(format!("{path}\n"))
                    }),
                );
                dir.add(
                    "hid",
                    SimpleFile::new_regular(self.fs.clone(), move || {
                        crate::acpi::with_engine(|e| {
                            e.hardware_id(&hidpath)
                                .map(|h| format!("{h}\n"))
                                .map_err(|_| VfsError::NotFound)
                        })
                        .unwrap_or(Err(VfsError::NotFound))
                    }),
                );
                dir.add(
                    "status",
                    SimpleFile::new_regular(self.fs.clone(), move || {
                        crate::acpi::with_engine(|e| match e.integer(&format!("{path}._STA")) {
                            Ok(v) => Ok(format!("{v}\n")),
                            Err(5) => Ok("15\n".into()),
                            Err(_) => Err(VfsError::Io),
                        })
                        .unwrap_or(Err(VfsError::NotFound))
                    }),
                );
                Ok(SimpleDir::new_maker(self.fs.clone(), Arc::new(dir)).into())
            }
        }
    }
    fn is_cacheable(&self) -> bool {
        false
    }
}
pub(super) fn firmware(fs: Arc<SimpleFs>) -> DirMaker {
    let mut acpi = DirMapping::new();
    acpi.add(
        "tables",
        SimpleDir::new_maker(
            fs.clone(),
            Arc::new(Directory {
                fs: fs.clone(),
                view: View::Tables,
            }),
        ),
    );
    let mut firmware = DirMapping::new();
    firmware.add("acpi", SimpleDir::new_maker(fs.clone(), Arc::new(acpi)));
    SimpleDir::new_maker(fs, Arc::new(firmware))
}
pub(super) fn bus_root(fs: Arc<SimpleFs>) -> DirMapping {
    let mut acpi = DirMapping::new();
    acpi.add(
        "devices",
        SimpleDir::new_maker(
            fs.clone(),
            Arc::new(Directory {
                fs: fs.clone(),
                view: View::Devices,
            }),
        ),
    );
    let mut bus = DirMapping::new();
    bus.add("acpi", SimpleDir::new_maker(fs, Arc::new(acpi)));
    bus
}
