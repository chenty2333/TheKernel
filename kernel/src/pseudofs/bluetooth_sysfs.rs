//! Linux-shaped sysfs class for USB HCI controllers registered by tk-axdriver.
use alloc::{borrow::Cow, format, string::String, sync::Arc, vec::Vec};

use axfs_ng_vfs::{FsName, FsNameBuf, VfsError, VfsResult};

use super::{
    ChildNames, DirMapping, NodeOpsMux, SimpleDir, SimpleDirOps, SimpleFile, SimpleFs,
    try_boxed_names,
};

struct BluetoothClass {
    fs: Arc<SimpleFs>,
}

fn adapter_index(name: &FsName) -> Option<u16> {
    let text = core::str::from_utf8(name.as_bytes()).ok()?;
    text.strip_prefix("hci")?.parse().ok()
}

impl SimpleDirOps for BluetoothClass {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        #[cfg(feature = "input")]
        {
            let devices = axdriver::bluetooth_devices();
            let mut names = Vec::new();
            names
                .try_reserve(devices.len())
                .map_err(|_| VfsError::NoMemory)?;
            for adapter in devices {
                let name = format!("hci{}", adapter.lock().index());
                names.push(Cow::Owned(FsNameBuf::from_vec(name.into_bytes())?));
            }
            try_boxed_names(names.into_iter())
        }
        #[cfg(not(feature = "input"))]
        {
            try_boxed_names(core::iter::empty())
        }
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let index = adapter_index(name).ok_or(VfsError::NotFound)?;
        #[cfg(feature = "input")]
        {
            let adapter = axdriver::bluetooth_devices()
                .into_iter()
                .find(|adapter| adapter.lock().index() == index)
                .ok_or(VfsError::NotFound)?;
            let mut files = DirMapping::new();
            let address = adapter.clone();
            files.add(
                "address",
                SimpleFile::new_regular(self.fs.clone(), move || {
                    let bytes = address.lock().address();
                    Ok(format!(
                        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}\n",
                        bytes[5], bytes[4], bytes[3], bytes[2], bytes[1], bytes[0]
                    ))
                }),
            );
            let name = format!("hci{index}");
            let device_name = name.clone();
            files.add(
                "name",
                SimpleFile::new_regular(self.fs.clone(), move || Ok(format!("{device_name}\n"))),
            );
            files.add(
                "dev_id",
                SimpleFile::new_regular(self.fs.clone(), move || Ok(format!("{index}\n"))),
            );
            files.add(
                "type",
                SimpleFile::new_regular(self.fs.clone(), || {
                    Ok::<String, VfsError>("Primary Controller\n".into())
                }),
            );
            files.add(
                "bus",
                SimpleFile::new_regular(self.fs.clone(), || Ok::<String, VfsError>("USB\n".into())),
            );
            files.add(
                "manufacturer",
                SimpleFile::new_regular(self.fs.clone(), || {
                    Ok::<String, VfsError>("Intel\n".into())
                }),
            );
            let feature_device = adapter.clone();
            files.add(
                "features",
                SimpleFile::new_regular(self.fs.clone(), move || {
                    let capabilities = feature_device.lock().capabilities();
                    let mut value = String::new();
                    for byte in capabilities.features {
                        use core::fmt::Write;
                        let _ = write!(value, "{byte:02x}");
                    }
                    value.push('\n');
                    Ok(value)
                }),
            );
            let version_device = adapter.clone();
            files.add(
                "hci_version",
                SimpleFile::new_regular(self.fs.clone(), move || {
                    Ok(format!(
                        "{}\n",
                        version_device.lock().capabilities().hci_version
                    ))
                }),
            );
            let revision_device = adapter.clone();
            files.add(
                "hci_revision",
                SimpleFile::new_regular(self.fs.clone(), move || {
                    Ok(format!(
                        "0x{:04x}\n",
                        revision_device.lock().capabilities().hci_revision
                    ))
                }),
            );
            let up = adapter.clone();
            files.add(
                "flags",
                SimpleFile::new_regular(self.fs.clone(), move || {
                    Ok::<String, VfsError>(
                        if up.lock().is_up() {
                            "0x0001\n"
                        } else {
                            "0x0000\n"
                        }
                        .into(),
                    )
                }),
            );
            Ok(SimpleDir::new_maker(self.fs.clone(), Arc::new(files)).into())
        }
        #[cfg(not(feature = "input"))]
        {
            let _ = index;
            Err(VfsError::NotFound)
        }
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

pub(super) fn class_root(fs: Arc<SimpleFs>) -> DirMapping {
    let mut root = DirMapping::new();
    root.add(
        "bluetooth",
        SimpleDir::new_maker(fs.clone(), Arc::new(BluetoothClass { fs })),
    );
    root
}
