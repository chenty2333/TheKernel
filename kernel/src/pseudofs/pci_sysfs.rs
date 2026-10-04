//! Real PCI identity/configuration through the existing device registry.
//! Linux 7.2.3 pci-sysfs.c/probe.c are behavior/format references only.
use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::{any::Any, task::Context};

use axdriver::pci::Address;
use axfs_ng_vfs::{
    FileNodeOps, FilesystemOps, FsPath, Metadata, MetadataUpdate, NodeFlags, NodeOps,
    NodePermission, NodeType, NodeUserData, VfsError, VfsResult,
};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use inherit_methods_macro::inherit_methods;

use super::{
    SimpleFs, SimpleFsNode,
    device_registry::{
        DeviceAttribute, DeviceHandle, DeviceIdentity, DeviceRegistration, MAX_DEVICES,
        global_device_registry,
    },
};

const FIELDS: [&str; 6] = [
    "vendor",
    "device",
    "class",
    "revision",
    "subsystem_vendor",
    "subsystem_device",
];
struct BootInventory {
    initialized: bool,
    handles: Vec<DeviceHandle<'static, MAX_DEVICES>>,
}
static BOOT_INVENTORY: Mutex<BootInventory> = Mutex::new(BootInventory {
    initialized: false,
    handles: Vec::new(),
});

fn field_text(header: &[u8; 64], subsystem: (u16, u16), name: &str) -> VfsResult<String> {
    let half = |offset| u16::from_le_bytes([header[offset], header[offset + 1]]);
    match name {
        "vendor" => Ok(format!("0x{:04x}\n", half(0))),
        "device" => Ok(format!("0x{:04x}\n", half(2))),
        "class" => Ok(format!(
            "0x{:06x}\n",
            u32::from(header[11]) << 16 | u32::from(header[10]) << 8 | u32::from(header[9])
        )),
        "revision" => Ok(format!("0x{:02x}\n", header[8])),
        "subsystem_vendor" => Ok(format!("0x{:04x}\n", subsystem.0)),
        "subsystem_device" => Ok(format!("0x{:04x}\n", subsystem.1)),
        _ => Err(VfsError::NotFound),
    }
}

fn prefix_limit(header_type: u8, privileged: bool) -> usize {
    if privileged {
        4096
    } else if header_type & 0x7f == 2 {
        128
    } else {
        64
    }
}
struct ConfigFile {
    node: SimpleFsNode,
    address: Address,
    header_type: u8,
    size: usize,
}
impl ConfigFile {
    fn try_new(
        fs: Arc<SimpleFs>,
        address: Address,
        header_type: u8,
        size: usize,
    ) -> VfsResult<Arc<dyn FileNodeOps>> {
        let node = SimpleFsNode::try_new(
            fs,
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o444),
        )?;
        node.metadata.lock().size = size as u64;
        Arc::try_new(Self {
            node,
            address,
            header_type,
            size,
        })
        .map(|file| file as Arc<dyn FileNodeOps>)
        .map_err(|_| VfsError::NoMemory)
    }
}
#[inherit_methods(from = "self.node")]
impl NodeOps for ConfigFile {
    fn inode(&self) -> u64;
    fn metadata(&self) -> VfsResult<Metadata> {
        let mut metadata = self.node.metadata.lock().clone();
        metadata.size = self.size as u64;
        Ok(metadata)
    }
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;
    fn filesystem(&self) -> &dyn FilesystemOps;
    fn sync(&self, data_only: bool) -> VfsResult<()>;
    fn len(&self) -> VfsResult<u64> {
        Ok(self.size as u64)
    }
    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::OPEN_CREDENTIAL
    }
    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(&self.node.user_data)
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}
impl FileNodeOps for ConfigFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let privileged =
            crate::file::current_file_operation_security_credential().is_some_and(|actor| {
                let initial = crate::task::security::initial_user_namespace(actor.user_ns());
                crate::task::ns_capable(&actor, &initial, linux_raw_sys::general::CAP_SYS_ADMIN)
            });
        let limit = prefix_limit(self.header_type, privileged).min(self.size);
        if offset >= limit as u64 || buf.is_empty() {
            return Ok(0);
        }
        let offset = offset as usize;
        let count = buf.len().min(limit - offset);
        axdriver::pci::read_configuration(self.address, offset, &mut buf[..count])
            .map_err(|_| VfsError::Io)
    }
    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::BadFileDescriptor)
    }
    fn append(&self, _buf: &[u8]) -> VfsResult<(usize, u64)> {
        Err(VfsError::BadFileDescriptor)
    }
    fn set_len(&self, _len: u64) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }
    fn set_symlink(&self, _target: &FsPath) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }
}
impl Pollable for ConfigFile {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

/// Existing DRM/input PCI parents receive the same real fields, not a second
/// overlapping kobject. Synthetic hosted fixtures retain their own attributes.
pub(super) fn enrich(
    name: &str,
    mut attributes: Vec<DeviceAttribute>,
) -> VfsResult<Vec<DeviceAttribute>> {
    let Some(address) =
        Address::parse(name).filter(|address| axdriver::pci::header(*address).is_some())
    else {
        return Ok(attributes);
    };
    attributes
        .try_reserve(FIELDS.len() + 3)
        .map_err(|_| VfsError::NoMemory)?;
    attributes.retain(|attribute| {
        !FIELDS.contains(&attribute.name())
            && !matches!(attribute.name(), "config" | "numa_node" | "irq")
    });
    for field in FIELDS {
        attributes.push(DeviceAttribute::try_new(field.into(), move || {
            let header = axdriver::pci::header(address).ok_or(VfsError::NotFound)?;
            let subsystem = axdriver::pci::subsystem(address, &header).ok_or(VfsError::Io)?;
            field_text(&header, subsystem, field)
        })?);
    }
    let header = axdriver::pci::header(address).ok_or(VfsError::NotFound)?;
    let size = axdriver::pci::configuration_size(address).ok_or(VfsError::Io)?;
    let header_type = header[14];
    attributes.push(DeviceAttribute::try_node("config".into(), move |fs| {
        ConfigFile::try_new(fs, address, header_type, size)
    })?);
    attributes.push(DeviceAttribute::try_new("irq".into(), move || {
        axdriver::pci::irq(address)
            .map(|irq| format!("{irq}\n"))
            .ok_or(VfsError::Io)
    })?);
    // No PCI-to-NUMA affinity has been discovered. Linux uses -1 for unknown.
    attributes.push(DeviceAttribute::try_new("numa_node".into(), || Ok("-1\n"))?);
    Ok(attributes)
}

/// Populate unowned boot PCI functions (including bridges) alongside, rather
/// than shadowing, already published DRM/input objects. The fixed registry
/// capacity remains fail-closed and is reported, not silently overrun.
pub(super) fn publish_inventory() {
    let mut boot = BOOT_INVENTORY.lock();
    if boot.initialized {
        return;
    }
    let inventory = match axdriver::pci::inventory() {
        Ok(inventory) => inventory,
        Err(error) => {
            warn!("PCI sysfs inventory unavailable: {error:?}");
            return;
        }
    };
    let registry = global_device_registry();
    let handles = &mut boot.handles;
    for address in inventory {
        let result = (|| -> VfsResult<()> {
            let identity = DeviceIdentity::without_dev(
                format!("pci{:04x}:{:02x}", address.segment, address.bus),
                "pci".into(),
                format!("{address}"),
            )?;
            let reservation = match registry.reserve(identity.clone()) {
                Ok(reservation) => reservation,
                Err(VfsError::AlreadyExists) => return Ok(()),
                Err(error) => return Err(error),
            };
            handles.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            let device = DeviceRegistration::try_bus_device(
                identity,
                "pci_device".into(),
                Vec::new(),
                "pci".into(),
                false,
            )?;
            handles.push(reservation.publish(device)?);
            Ok(())
        })();
        if let Err(error) = result {
            warn!("PCI sysfs publication at {address} failed: {error}");
            break;
        }
    }
    // Later sysfs mounts must not claim a just-arrived input function before
    // the existing PCI-input reconcile owner publishes its parent kobject.
    boot.initialized = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_config_node_retains_open_credentials_and_full_metadata_length() {
        let _context = crate::test_support::scheduler_test_context();
        let _filesystem = SimpleFs::new_with("test".into(), 0, |fs| {
            let file =
                ConfigFile::try_new(fs.clone(), Address::parse("0000:00:01.0").unwrap(), 0, 256)
                    .unwrap();
            assert!(file.flags().contains(NodeFlags::OPEN_CREDENTIAL));
            assert_eq!(file.metadata().unwrap().mode.bits(), 0o444);
            assert_eq!(file.metadata().unwrap().size, 256);
            assert_eq!(file.len().unwrap(), 256);
            assert_eq!(file.write_at(&[0], 0), Err(VfsError::BadFileDescriptor));
            assert_eq!(file.read_at(&mut [0; 1], 64).unwrap(), 0);
            super::super::SimpleDir::new_maker(fs, Arc::new(super::super::DirMapping::new()))
        });
    }
    #[test]
    fn identity_fields_use_linux_hex_widths_and_class_byte_order() {
        let mut header = [0; 64];
        header[..4].copy_from_slice(&[0xf4, 0x1a, 0x10, 0x10]);
        header[8] = 5;
        header[9] = 0x30;
        header[10] = 3;
        header[11] = 0x0c;
        for (name, expected) in [
            ("vendor", "0x1af4\n"),
            ("device", "0x1010\n"),
            ("class", "0x0c0330\n"),
            ("revision", "0x05\n"),
            ("subsystem_vendor", "0x1234\n"),
            ("subsystem_device", "0x5678\n"),
        ] {
            assert_eq!(
                field_text(&header, (0x1234, 0x5678), name).unwrap(),
                expected
            );
        }
    }
    #[test]
    fn config_prefix_depends_on_header_and_initial_namespace_privilege() {
        assert_eq!(prefix_limit(0, false), 64);
        assert_eq!(prefix_limit(1, false), 64);
        assert_eq!(prefix_limit(0x82, false), 128);
        assert_eq!(prefix_limit(0, true), 4096);
    }
}
