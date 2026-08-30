//! Dynamically published sysfs device objects.
//!
//! A device is invisible until its caller has reserved a fixed registry slot,
//! built its immutable description, and committed that description.  All sysfs
//! views consult the same registry, so publication/removal has one visibility
//! point rather than separate updates to `/sys/devices`, `/sys/class`, and
//! `/sys/dev/char`.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use axfs_ng_vfs::{DeviceId, VfsError, VfsResult};
use axsync::Mutex;
use lazy_static::lazy_static;

use super::{
    ChildNames, DirMaker, NodeOpsMux, RwFile, SimpleDir, SimpleDirOps, SimpleFile,
    SimpleFileOperation, SimpleFileOps, SimpleFs, try_boxed_names,
};

/// The registry intentionally has a fixed admission bound.  Drivers must
/// handle `ResourceBusy` rather than letting device discovery consume memory
/// without limit.
pub const MAX_DEVICES: usize = 64;
pub const MAX_DEVICE_ATTRIBUTES: usize = 32;

lazy_static! {
    static ref DEVICE_REGISTRY: DeviceRegistry<MAX_DEVICES> = DeviceRegistry::new();
}

pub fn global_device_registry() -> &'static DeviceRegistry<MAX_DEVICES> {
    &DEVICE_REGISTRY
}

/// Identity shared by all sysfs presentations of a device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceIdentity {
    pub bus: String,
    pub class: String,
    pub name: String,
    pub device_id: Option<DeviceId>,
    pub devname: Option<String>,
    pub parent: Option<(String, String)>,
}

impl DeviceIdentity {
    pub fn new(bus: String, class: String, name: String, device_id: DeviceId) -> VfsResult<Self> {
        validate_component(&bus)?;
        validate_component(&class)?;
        validate_component(&name)?;
        let devname = name.clone();
        Ok(Self {
            bus,
            class,
            name,
            device_id: Some(device_id),
            devname: Some(devname),
            parent: None,
        })
    }

    /// A sysfs-only device object.  Such objects intentionally do not appear
    /// under `/sys/dev/char` and do not carry MAJOR/MINOR uevent fields.
    pub fn without_dev(bus: String, class: String, name: String) -> VfsResult<Self> {
        validate_component(&bus)?;
        validate_component(&class)?;
        validate_component(&name)?;
        Ok(Self {
            bus,
            class,
            name,
            device_id: None,
            devname: None,
            parent: None,
        })
    }

    /// Sets the `/dev`-relative device node name independently of the sysfs
    /// kobject name.  Subdirectories are valid here (`input/event0`), unlike
    /// sysfs components.
    pub fn with_devname(mut self, devname: String) -> VfsResult<Self> {
        validate_devname(&devname)?;
        if self.device_id.is_none() {
            return Err(VfsError::InvalidInput);
        }
        self.devname = Some(devname);
        Ok(self)
    }

    pub fn child_of(mut self, bus: String, name: String) -> VfsResult<Self> {
        validate_component(&bus)?;
        validate_component(&name)?;
        self.parent = Some((bus, name));
        Ok(self)
    }
}

/// Event requested by a write to the device's `uevent` attribute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceUeventAction {
    Add,
    Remove,
    Change,
}

pub trait DeviceUeventHook: Send + Sync + 'static {
    fn trigger(&self, action: DeviceUeventAction) -> VfsResult<()>;
}

impl<F> DeviceUeventHook for F
where
    F: Fn(DeviceUeventAction) -> VfsResult<()> + Send + Sync + 'static,
{
    fn trigger(&self, action: DeviceUeventAction) -> VfsResult<()> {
        self(action)
    }
}

/// An immutable device attribute.  Use a fallible `Arc` allocation while the
/// device is being constructed, before it can be observed by path lookup.
#[derive(Clone)]
pub struct DeviceAttribute {
    name: String,
    kind: DeviceAttributeKind,
}

#[derive(Clone)]
enum DeviceAttributeKind {
    File(Arc<dyn SimpleFileOps>),
    Directory(Arc<Vec<DeviceAttribute>>),
}

impl DeviceAttribute {
    pub fn try_new(name: String, ops: impl SimpleFileOps) -> VfsResult<Self> {
        validate_component(&name)?;
        let ops = Arc::try_new(ops).map_err(|_| VfsError::NoMemory)?;
        Ok(Self {
            name,
            kind: DeviceAttributeKind::File(ops),
        })
    }

    /// Creates a static sysfs attribute directory.  Attribute trees are part
    /// of the immutable registration, so lookup never observes a partially
    /// populated directory.
    pub fn try_directory(name: String, children: Vec<DeviceAttribute>) -> VfsResult<Self> {
        validate_component(&name)?;
        validate_attribute_names(&children)?;
        let children = Arc::try_new(children).map_err(|_| VfsError::NoMemory)?;
        Ok(Self {
            name,
            kind: DeviceAttributeKind::Directory(children),
        })
    }

    #[cfg(test)]
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    #[cfg(test)]
    pub(crate) fn directory_child_names(&self) -> Option<Vec<&str>> {
        match &self.kind {
            DeviceAttributeKind::File(_) => None,
            DeviceAttributeKind::Directory(children) => Some(
                children
                    .iter()
                    .map(|attribute| attribute.name.as_str())
                    .collect(),
            ),
        }
    }
}

/// Complete immutable input for one registry publication.
pub struct DeviceRegistration {
    identity: DeviceIdentity,
    devtype: String,
    attributes: Vec<DeviceAttribute>,
    uevent_hook: Option<Arc<dyn DeviceUeventHook>>,
}

impl DeviceRegistration {
    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    pub fn try_new(
        identity: DeviceIdentity,
        devtype: String,
        attributes: Vec<DeviceAttribute>,
        uevent_hook: Option<Arc<dyn DeviceUeventHook>>,
    ) -> VfsResult<Arc<Self>> {
        validate_component(&devtype)?;
        match (&identity.device_id, &identity.devname) {
            (Some(_), Some(devname)) => validate_devname(devname)?,
            (None, None) => {}
            _ => return Err(VfsError::InvalidInput),
        }
        if attributes.len() > MAX_DEVICE_ATTRIBUTES
            || attributes.iter().any(|attr| {
                matches!(
                    attr.name.as_str(),
                    "dev" | "uevent" | "subsystem" | "device"
                )
            })
        {
            return Err(VfsError::InvalidInput);
        }
        validate_attribute_names(&attributes)?;
        Arc::try_new(Self {
            identity,
            devtype,
            attributes,
            uevent_hook,
        })
        .map_err(|_| VfsError::NoMemory)
    }

    fn canonical_path(&self) -> String {
        // Capacity is bounded by the validated strings, and this path is only
        // rendered after the immutable registration is visible.
        match &self.identity.parent {
            Some((bus, name)) => alloc::format!("/devices/{bus}/{name}/{}", self.identity.name),
            None => alloc::format!("/devices/{}/{}", self.identity.bus, self.identity.name),
        }
    }
}

fn validate_attribute_names(attributes: &[DeviceAttribute]) -> VfsResult<()> {
    for (index, attr) in attributes.iter().enumerate() {
        if attributes[..index]
            .iter()
            .any(|other| other.name == attr.name)
        {
            return Err(VfsError::AlreadyExists);
        }
    }
    Ok(())
}

enum Slot {
    Vacant,
    Reserved {
        generation: u64,
        identity: DeviceIdentity,
    },
    Published {
        generation: u64,
        device: Arc<DeviceRegistration>,
    },
    /// A removal owns this slot, but leaves every sysfs view resolvable until
    /// its `remove` uevent has been delivered.
    Disconnecting {
        generation: u64,
        device: Arc<DeviceRegistration>,
    },
}

/// Fixed-capacity, two-phase device registry.
pub struct DeviceRegistry<const CAPACITY: usize> {
    slots: Mutex<[Slot; CAPACITY]>,
    next_generation: Mutex<u64>,
}

impl<const CAPACITY: usize> DeviceRegistry<CAPACITY> {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(core::array::from_fn(|_| Slot::Vacant)),
            next_generation: Mutex::new(1),
        }
    }

    /// Reserve the identity before allocating/building the published object.
    pub fn reserve(&self, identity: DeviceIdentity) -> VfsResult<DeviceReservation<'_, CAPACITY>> {
        let mut slots = self.slots.lock();
        if slots.iter().any(|slot| match slot {
            Slot::Reserved {
                identity: existing, ..
            } => same_identity(existing, &identity),
            Slot::Published {
                device: existing, ..
            } => same_identity(&existing.identity, &identity),
            Slot::Disconnecting {
                device: existing, ..
            } => same_identity(&existing.identity, &identity),
            Slot::Vacant => false,
        }) {
            return Err(VfsError::AlreadyExists);
        }
        let index = slots
            .iter()
            .position(|slot| matches!(slot, Slot::Vacant))
            .ok_or(VfsError::ResourceBusy)?;
        let mut generation = self.next_generation.lock();
        let token = *generation;
        *generation = generation.wrapping_add(1).max(1);
        slots[index] = Slot::Reserved {
            generation: token,
            identity,
        };
        Ok(DeviceReservation {
            registry: self,
            index,
            generation: token,
            consumed: false,
        })
    }

    fn visible_matching(
        &self,
        predicate: impl Fn(&DeviceRegistration) -> bool,
    ) -> Vec<Arc<DeviceRegistration>> {
        self.slots
            .lock()
            .iter()
            .filter_map(|slot| match slot {
                Slot::Published { device, .. } if predicate(device) => Some(device.clone()),
                Slot::Disconnecting { device, .. } if predicate(device) => Some(device.clone()),
                _ => None,
            })
            .collect()
    }

    fn device(&self, bus: &str, name: &str) -> Option<Arc<DeviceRegistration>> {
        self.visible_matching(|device| device.identity.bus == bus && device.identity.name == name)
            .pop()
    }

    fn root_device(&self, bus: &str, name: &str) -> Option<Arc<DeviceRegistration>> {
        self.visible_matching(|device| {
            device.identity.bus == bus
                && device.identity.name == name
                && device.identity.parent.is_none()
        })
        .pop()
    }

    fn begin_disconnect(
        &self,
        index: usize,
        generation: u64,
    ) -> VfsResult<Arc<DeviceRegistration>> {
        let mut slots = self.slots.lock();
        let Some(slot) = slots.get(index) else {
            return Err(VfsError::NotFound);
        };
        let Slot::Published {
            generation: current_generation,
            device,
        } = slot
        else {
            return Err(VfsError::NotFound);
        };
        if *current_generation != generation {
            return Err(VfsError::NotFound);
        }
        let device = device.clone();
        slots[index] = Slot::Disconnecting {
            generation,
            device: device.clone(),
        };
        Ok(device)
    }

    fn finish_disconnect(&self, index: usize, generation: u64) -> VfsResult<()> {
        let mut slots = self.slots.lock();
        if matches!(slots.get(index), Some(Slot::Disconnecting { generation: current_generation, .. }) if *current_generation == generation)
        {
            slots[index] = Slot::Vacant;
            Ok(())
        } else {
            Err(VfsError::NotFound)
        }
    }
}

impl DeviceRegistration {
    /// Emit the Linux device event after the registry lock has been released.
    fn emit_uevent(&self, action: DeviceUeventAction) -> VfsResult<()> {
        let action = match action {
            DeviceUeventAction::Add => "add",
            DeviceUeventAction::Remove => "remove",
            DeviceUeventAction::Change => "change",
        };
        let devpath = self.canonical_path();
        let mut environment = Vec::new();
        if let Some(device_id) = self.identity.device_id {
            environment.push(("MAJOR", device_id.major().to_string()));
            environment.push(("MINOR", device_id.minor().to_string()));
            environment.push((
                "DEVNAME",
                self.identity
                    .devname
                    .clone()
                    .ok_or(VfsError::InvalidInput)?,
            ));
        }
        environment.push(("DEVTYPE", self.devtype.clone()));
        let environment = environment
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect::<Vec<_>>();
        // Linux device discovery is global to init-net, never the namespace
        // of the task that happened to trigger this sysfs attribute write.
        // Entry establishes init-net before any device can be published.
        crate::file::netlink::emit_init_net_kobject_uevent(
            action,
            &devpath,
            &self.identity.class,
            &environment,
        )?;
        Ok(())
    }

    pub(crate) fn uevent_payload(&self) -> String {
        let mut payload = alloc::format!(
            "DEVPATH={}\nSUBSYSTEM={}\nDEVTYPE={}\n",
            self.canonical_path(),
            self.identity.class,
            self.devtype,
        );
        if let Some(device_id) = self.identity.device_id {
            payload = alloc::format!(
                "MAJOR={}\nMINOR={}\nDEVNAME={}\n{}",
                device_id.major(),
                device_id.minor(),
                self.identity
                    .devname
                    .as_deref()
                    .expect("devname validated at registration"),
                payload,
            );
        }
        payload
    }

    fn trigger_uevent(&self, action: DeviceUeventAction) -> VfsResult<()> {
        if let Some(hook) = &self.uevent_hook {
            hook.trigger(action)?;
        }
        self.emit_uevent(action)
    }
}

fn same_identity(left: &DeviceIdentity, right: &DeviceIdentity) -> bool {
    left.bus == right.bus && left.name == right.name
        || left.class == right.class && left.name == right.name
        || left.device_id.is_some() && left.device_id == right.device_id
}

/// An admission reservation.  Dropping it rolls back an unfinished publish.
pub struct DeviceReservation<'a, const CAPACITY: usize> {
    registry: &'a DeviceRegistry<CAPACITY>,
    index: usize,
    generation: u64,
    consumed: bool,
}

impl<'a, const CAPACITY: usize> DeviceReservation<'a, CAPACITY> {
    /// Publishes a bounded related device set at one visibility point.  Every
    /// reservation is checked while the registry lock is held before any slot
    /// changes state, so a failed three-node DRM publication has no visible
    /// card/connector/render fragment.
    pub fn publish_many<const COUNT: usize>(
        mut entries: [(Self, Arc<DeviceRegistration>); COUNT],
    ) -> VfsResult<[DeviceHandle<'a, CAPACITY>; COUNT]> {
        if COUNT == 0 {
            return Err(VfsError::InvalidInput);
        }
        let registry = entries[0].0.registry;
        if entries
            .iter()
            .any(|(reservation, _)| !core::ptr::eq(registry, reservation.registry))
            || entries.iter().enumerate().any(|(index, (reservation, _))| {
                entries[..index]
                    .iter()
                    .any(|(other, _)| other.index == reservation.index)
            })
        {
            return Err(VfsError::InvalidInput);
        }
        let mut slots = registry.slots.lock();
        for (reservation, device) in &entries {
            let Some(Slot::Reserved {
                generation,
                identity,
            }) = slots.get(reservation.index)
            else {
                return Err(VfsError::NotFound);
            };
            if *generation != reservation.generation || identity != &device.identity {
                return Err(VfsError::InvalidInput);
            }
        }
        for (reservation, device) in &entries {
            slots[reservation.index] = Slot::Published {
                generation: reservation.generation,
                device: device.clone(),
            };
        }
        for (reservation, _) in &mut entries {
            reservation.consumed = true;
        }
        let handles = core::array::from_fn(|index| DeviceHandle {
            registry,
            index: entries[index].0.index,
            generation: entries[index].0.generation,
        });
        drop(slots);
        for (_, device) in entries {
            let _ = device.emit_uevent(DeviceUeventAction::Add);
        }
        Ok(handles)
    }

    /// Publishes two related sysfs objects at one visibility point.  This is
    /// used for device/child pairs so an allocation or validation failure
    /// cannot leave only one half of the hierarchy visible.
    pub fn publish_pair(
        mut first: Self,
        first_device: Arc<DeviceRegistration>,
        mut second: Self,
        second_device: Arc<DeviceRegistration>,
    ) -> VfsResult<(DeviceHandle<'a, CAPACITY>, DeviceHandle<'a, CAPACITY>)> {
        if !core::ptr::eq(first.registry, second.registry) || first.index == second.index {
            return Err(VfsError::InvalidInput);
        }
        let registry = first.registry;
        let mut slots = registry.slots.lock();
        for (reservation, device) in [(&first, &first_device), (&second, &second_device)] {
            let Some(Slot::Reserved {
                generation,
                identity,
            }) = slots.get(reservation.index)
            else {
                return Err(VfsError::NotFound);
            };
            if *generation != reservation.generation || identity != &device.identity {
                return Err(VfsError::InvalidInput);
            }
        }
        slots[first.index] = Slot::Published {
            generation: first.generation,
            device: first_device.clone(),
        };
        slots[second.index] = Slot::Published {
            generation: second.generation,
            device: second_device.clone(),
        };
        first.consumed = true;
        second.consumed = true;
        drop(slots);
        let _ = first_device.emit_uevent(DeviceUeventAction::Add);
        let _ = second_device.emit_uevent(DeviceUeventAction::Add);
        Ok((
            DeviceHandle {
                registry,
                index: first.index,
                generation: first.generation,
            },
            DeviceHandle {
                registry,
                index: second.index,
                generation: second.generation,
            },
        ))
    }

    /// Atomically exposes a fully constructed device in all dynamic sysfs
    /// lookup domains.
    pub fn publish(
        mut self,
        device: Arc<DeviceRegistration>,
    ) -> VfsResult<DeviceHandle<'a, CAPACITY>> {
        // Retain the immutable description before taking the registry lock so
        // rendering/broadcasting the event cannot occur while it is held.
        let notification_device = device.clone();
        let mut slots = self.registry.slots.lock();
        let Slot::Reserved {
            generation,
            identity,
        } = &slots[self.index]
        else {
            return Err(VfsError::NotFound);
        };
        if *generation != self.generation || identity != &device.identity {
            return Err(VfsError::InvalidInput);
        }
        slots[self.index] = Slot::Published {
            generation: self.generation,
            device,
        };
        self.consumed = true;
        let handle = DeviceHandle {
            registry: self.registry,
            index: self.index,
            generation: self.generation,
        };
        drop(slots);
        // Publication is already committed.  A best-effort multicast failure
        // must not report an invisible failure while retaining the device.
        let _ = notification_device.emit_uevent(DeviceUeventAction::Add);
        Ok(handle)
    }
}

impl<const CAPACITY: usize> Drop for DeviceReservation<'_, CAPACITY> {
    fn drop(&mut self) {
        if self.consumed {
            return;
        }
        let mut slots = self.registry.slots.lock();
        if matches!(&slots[self.index], Slot::Reserved { generation, .. } if *generation == self.generation)
        {
            slots[self.index] = Slot::Vacant;
        }
    }
}

/// Capability used to atomically remove the exact published device.
#[derive(Clone, Copy)]
pub struct DeviceHandle<'a, const CAPACITY: usize> {
    registry: &'a DeviceRegistry<CAPACITY>,
    index: usize,
    generation: u64,
}

impl<const CAPACITY: usize> DeviceHandle<'_, CAPACITY> {
    pub fn remove(self) -> VfsResult<()> {
        let device = self
            .registry
            .begin_disconnect(self.index, self.generation)?;

        // The disconnecting state preserves all attributes while `remove` is
        // broadcast.  Event construction, allocation, and socket wakeups are
        // deliberately outside the registry lock.
        let notification_result = device.emit_uevent(DeviceUeventAction::Remove);
        self.registry
            .finish_disconnect(self.index, self.generation)?;
        notification_result
    }
}

fn validate_component(value: &str) -> VfsResult<()> {
    if value.is_empty()
        || value.len() > 255
        || value.as_bytes().iter().any(|byte| matches!(byte, b'/' | 0))
    {
        return Err(VfsError::InvalidInput);
    }
    Ok(())
}

fn validate_devname(value: &str) -> VfsResult<()> {
    if value.is_empty()
        || value.len() > 255
        || value.starts_with('/')
        || value.as_bytes().iter().any(|byte| *byte == 0)
    {
        return Err(VfsError::InvalidInput);
    }
    for component in value.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(VfsError::InvalidInput);
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct RegistryDir {
    fs: Arc<SimpleFs>,
    kind: RegistryDirKind,
}
#[derive(Clone)]
enum RegistryDirKind {
    ClassRoot,
    Class(String),
    DevCharRoot,
    DevicesRoot,
    Bus(String),
    Device(String, String),
}

pub fn class_root(fs: Arc<SimpleFs>) -> RegistryDir {
    RegistryDir {
        fs,
        kind: RegistryDirKind::ClassRoot,
    }
}
pub fn dev_char_root(fs: Arc<SimpleFs>) -> RegistryDir {
    RegistryDir {
        fs,
        kind: RegistryDirKind::DevCharRoot,
    }
}
pub fn devices_root(fs: Arc<SimpleFs>) -> RegistryDir {
    RegistryDir {
        fs,
        kind: RegistryDirKind::DevicesRoot,
    }
}

impl RegistryDir {
    fn maker(&self, kind: RegistryDirKind) -> DirMaker {
        let fs = self.fs.clone();
        Arc::new(move |this| {
            SimpleDir::new_maker(
                fs.clone(),
                Arc::new(RegistryDir {
                    fs: fs.clone(),
                    kind: kind.clone(),
                }),
            )(this)
        })
    }
    fn device_from_kind(&self) -> Option<Arc<DeviceRegistration>> {
        let RegistryDirKind::Device(bus, name) = &self.kind else {
            return None;
        };
        global_device_registry().device(bus, name)
    }
}

impl SimpleDirOps for RegistryDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let devices = global_device_registry().visible_matching(|_| true);
        let mut names = Vec::new();
        for device in devices {
            let candidate = match &self.kind {
                RegistryDirKind::ClassRoot => Some(device.identity.class.clone()),
                RegistryDirKind::Class(class) if *class == device.identity.class => {
                    Some(device.identity.name.clone())
                }
                RegistryDirKind::DevCharRoot => device
                    .identity
                    .device_id
                    .map(|device_id| alloc::format!("{}:{}", device_id.major(), device_id.minor())),
                RegistryDirKind::DevicesRoot if device.identity.parent.is_none() => {
                    Some(device.identity.bus.clone())
                }
                RegistryDirKind::Bus(bus)
                    if *bus == device.identity.bus && device.identity.parent.is_none() =>
                {
                    Some(device.identity.name.clone())
                }
                RegistryDirKind::Device(..) => None,
                _ => None,
            };
            if let Some(candidate) = candidate {
                if !names.iter().any(|name| name == &candidate) {
                    names.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                    names.push(candidate);
                }
            }
        }
        if let Some(device) = self.device_from_kind() {
            let RegistryDirKind::Device(bus, name) = &self.kind else {
                unreachable!()
            };
            for child in global_device_registry().visible_matching(|candidate| {
                candidate.identity.parent.as_ref() == Some(&(bus.clone(), name.clone()))
            }) {
                names.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                names.push(child.identity.name.clone());
            }
            names
                .try_reserve(device.attributes.len() + 4)
                .map_err(|_| VfsError::NoMemory)?;
            if device.identity.device_id.is_some() {
                names.push("dev".into());
            }
            names.push("uevent".into());
            names.push("subsystem".into());
            names.push("device".into());
            for attribute in &device.attributes {
                names.push(attribute.name.clone());
            }
        }
        try_boxed_names(names.into_iter().map(Cow::Owned))
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        match &self.kind {
            RegistryDirKind::ClassRoot => {
                let exists = global_device_registry()
                    .visible_matching(|d| d.identity.class == name)
                    .len()
                    != 0;
                exists
                    .then(|| self.maker(RegistryDirKind::Class(name.into())).into())
                    .ok_or(VfsError::NotFound)
            }
            RegistryDirKind::Class(class) => {
                let device = global_device_registry()
                    .visible_matching(|d| d.identity.class == *class && d.identity.name == name)
                    .pop()
                    .ok_or(VfsError::NotFound)?;
                Ok(
                    SimpleFile::new(self.fs.clone(), axfs_ng_vfs::NodeType::Symlink, move || {
                        Ok(alloc::format!("../..{}", device.canonical_path()))
                    })
                    .into(),
                )
            }
            RegistryDirKind::DevCharRoot => {
                let device = global_device_registry()
                    .visible_matching(|d| {
                        d.identity.device_id.is_some_and(|device_id| {
                            alloc::format!("{}:{}", device_id.major(), device_id.minor()) == name
                        })
                    })
                    .pop()
                    .ok_or(VfsError::NotFound)?;
                Ok(
                    SimpleFile::new(self.fs.clone(), axfs_ng_vfs::NodeType::Symlink, move || {
                        Ok(alloc::format!("../..{}", device.canonical_path()))
                    })
                    .into(),
                )
            }
            RegistryDirKind::DevicesRoot => {
                let exists = global_device_registry()
                    .visible_matching(|d| d.identity.bus == name && d.identity.parent.is_none())
                    .len()
                    != 0;
                exists
                    .then(|| self.maker(RegistryDirKind::Bus(name.into())).into())
                    .ok_or(VfsError::NotFound)
            }
            RegistryDirKind::Bus(bus) => {
                let exists = global_device_registry().root_device(bus, name).is_some();
                exists
                    .then(|| {
                        self.maker(RegistryDirKind::Device(bus.clone(), name.into()))
                            .into()
                    })
                    .ok_or(VfsError::NotFound)
            }
            RegistryDirKind::Device(bus, parent) => {
                if let Some(child) = global_device_registry()
                    .visible_matching(|candidate| {
                        candidate.identity.bus == *bus
                            && candidate.identity.name == name
                            && candidate.identity.parent.as_ref()
                                == Some(&(bus.clone(), parent.clone()))
                    })
                    .pop()
                {
                    return Ok(self
                        .maker(RegistryDirKind::Device(
                            bus.clone(),
                            child.identity.name.clone(),
                        ))
                        .into());
                }
                self.device_file(name)
            }
        }
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

impl RegistryDir {
    fn device_file(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let device = self.device_from_kind().ok_or(VfsError::NotFound)?;
        if name == "dev" {
            let device_id = device.identity.device_id.ok_or(VfsError::NotFound)?;
            return Ok(SimpleFile::new_regular(self.fs.clone(), move || {
                Ok(alloc::format!(
                    "{}:{}\n",
                    device_id.major(),
                    device_id.minor()
                ))
            })
            .into());
        }
        if name == "uevent" {
            let read_device = device.clone();
            return Ok(SimpleFile::new_regular(
                self.fs.clone(),
                RwFile::new_root_writable(move |operation| match operation {
                    SimpleFileOperation::Read => {
                        Ok(Some(read_device.uevent_payload().into_bytes()))
                    }
                    SimpleFileOperation::Write(value) => {
                        let text = core::str::from_utf8(value)
                            .map_err(|_| VfsError::InvalidInput)?
                            .trim();
                        let action = match text {
                            "add" => DeviceUeventAction::Add,
                            "remove" => DeviceUeventAction::Remove,
                            "change" => DeviceUeventAction::Change,
                            _ => return Err(VfsError::InvalidInput),
                        };
                        read_device.trigger_uevent(action)?;
                        Ok(None::<Vec<u8>>)
                    }
                }),
            )
            .into());
        }
        if name == "subsystem" {
            let target = subsystem_link_target(&device.identity);
            return Ok(SimpleFile::new(
                self.fs.clone(),
                axfs_ng_vfs::NodeType::Symlink,
                move || Ok(target.clone()),
            )
            .into());
        }
        if name == "device" {
            // A class device's conventional `device` link points to its
            // parent kobject.  For top-level registered devices the bus
            // directory is that parent; nested input event devices resolve to
            // their inputN parent.
            return Ok(
                SimpleFile::new(self.fs.clone(), axfs_ng_vfs::NodeType::Symlink, || {
                    Ok::<String, VfsError>(device_link_target())
                })
                .into(),
            );
        }
        let attribute = device
            .attributes
            .iter()
            .find(|attribute| attribute.name == name)
            .ok_or(VfsError::NotFound)?;
        attribute_node(self.fs.clone(), attribute)
    }
}

/// `subsystem` is relative to the canonical `/sys/devices` kobject, not the
/// `/sys/class` symlink through which callers commonly reach it.
fn subsystem_link_target(identity: &DeviceIdentity) -> String {
    let levels_to_sys = if identity.parent.is_some() { 4 } else { 3 };
    alloc::format!("{}class/{}", "../".repeat(levels_to_sys), identity.class)
}

/// A canonical device object's parent is the `device` link target.  This is
/// deliberately class-agnostic: nested input devices point at inputN while
/// top-level DRM/fb devices point at their bus object.
fn device_link_target() -> String {
    "..".into()
}

fn attribute_node(fs: Arc<SimpleFs>, attribute: &DeviceAttribute) -> VfsResult<NodeOpsMux> {
    match &attribute.kind {
        DeviceAttributeKind::File(ops) => {
            Ok(SimpleFile::new_regular(fs, DeviceAttributeFile { ops: ops.clone() }).into())
        }
        DeviceAttributeKind::Directory(attributes) => Ok(SimpleDir::new_maker(
            fs.clone(),
            Arc::new(DeviceAttributeDir {
                fs,
                attributes: attributes.clone(),
            }),
        )
        .into()),
    }
}

struct DeviceAttributeDir {
    fs: Arc<SimpleFs>,
    attributes: Arc<Vec<DeviceAttribute>>,
}

impl SimpleDirOps for DeviceAttributeDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        try_boxed_names(
            self.attributes
                .iter()
                .map(|attribute| attribute.name.as_str().into()),
        )
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let attribute = self
            .attributes
            .iter()
            .find(|attribute| attribute.name == name)
            .ok_or(VfsError::NotFound)?;
        attribute_node(self.fs.clone(), attribute)
    }
}

struct DeviceAttributeFile {
    ops: Arc<dyn SimpleFileOps>,
}
impl SimpleFileOps for DeviceAttributeFile {
    fn default_permission(&self) -> axfs_ng_vfs::NodePermission {
        self.ops.default_permission()
    }
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        self.ops.read_all()
    }
    fn write_all(&self, data: &[u8]) -> VfsResult<()> {
        self.ops.write_all(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_from(link: &str, target: &str) -> String {
        let mut components = Vec::new();
        for component in link
            .rsplit_once('/')
            .unwrap()
            .0
            .split('/')
            .chain(target.split('/'))
        {
            match component {
                "" | "." => {}
                ".." => {
                    components.pop();
                }
                component => components.push(component),
            }
        }
        alloc::format!("/{}", components.join("/"))
    }

    fn registration(name: &str) -> Arc<DeviceRegistration> {
        DeviceRegistration::try_new(
            DeviceIdentity::new(
                "virtual".into(),
                "input".into(),
                name.into(),
                DeviceId::new(13, 1),
            )
            .unwrap(),
            "input".into(),
            Vec::new(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn reservation_is_invisible_until_atomic_publish_and_drop_rolls_back() {
        let registry = DeviceRegistry::<1>::new();
        let identity = DeviceIdentity::new(
            "virtual".into(),
            "input".into(),
            "event0".into(),
            DeviceId::new(13, 1),
        )
        .unwrap();
        let reservation = registry.reserve(identity).unwrap();
        assert!(registry.device("virtual", "event0").is_none());
        drop(reservation);
        assert!(registry.device("virtual", "event0").is_none());
        let reservation = registry
            .reserve(
                DeviceIdentity::new(
                    "virtual".into(),
                    "input".into(),
                    "event0".into(),
                    DeviceId::new(13, 1),
                )
                .unwrap(),
            )
            .unwrap();
        reservation.publish(registration("event0")).unwrap();
        assert!(registry.device("virtual", "event0").is_some());
    }

    #[test]
    fn identity_rejects_paths_and_duplicate_views() {
        assert_eq!(
            DeviceIdentity::new(
                "virtual/input".into(),
                "input".into(),
                "event0".into(),
                DeviceId::new(13, 1)
            ),
            Err(VfsError::InvalidInput)
        );
        let registry = DeviceRegistry::<2>::new();
        registry
            .reserve(
                DeviceIdentity::new(
                    "virtual".into(),
                    "input".into(),
                    "event0".into(),
                    DeviceId::new(13, 1),
                )
                .unwrap(),
            )
            .unwrap();
        assert!(matches!(
            registry.reserve(
                DeviceIdentity::new(
                    "other".into(),
                    "input".into(),
                    "event0".into(),
                    DeviceId::new(13, 2)
                )
                .unwrap()
            ),
            Err(VfsError::AlreadyExists)
        ));
    }

    #[test]
    fn removal_handle_cannot_remove_a_reused_slot() {
        let registry = DeviceRegistry::<1>::new();
        let identity = DeviceIdentity::new(
            "virtual".into(),
            "input".into(),
            "event0".into(),
            DeviceId::new(13, 1),
        )
        .unwrap();
        let handle = registry
            .reserve(identity)
            .unwrap()
            .publish(registration("event0"))
            .unwrap();
        let stale = handle;
        handle.remove().unwrap();
        let replacement = registry
            .reserve(
                DeviceIdentity::new(
                    "virtual".into(),
                    "input".into(),
                    "event0".into(),
                    DeviceId::new(13, 1),
                )
                .unwrap(),
            )
            .unwrap()
            .publish(registration("event0"))
            .unwrap();
        assert_eq!(stale.remove(), Err(VfsError::NotFound));
        assert!(registry.device("virtual", "event0").is_some());
        replacement.remove().unwrap();
    }

    #[test]
    fn disconnecting_keeps_names_reserved_and_visible_until_remove_finishes() {
        let registry = DeviceRegistry::<1>::new();
        let identity = DeviceIdentity::new(
            "virtual".into(),
            "input".into(),
            "event0".into(),
            DeviceId::new(13, 1),
        )
        .unwrap();
        let handle = registry
            .reserve(identity)
            .unwrap()
            .publish(registration("event0"))
            .unwrap();

        let device = registry
            .begin_disconnect(handle.index, handle.generation)
            .unwrap();
        assert_eq!(device.identity.name, "event0");
        assert!(registry.device("virtual", "event0").is_some());
        assert!(matches!(
            registry.reserve(
                DeviceIdentity::new(
                    "virtual".into(),
                    "input".into(),
                    "event0".into(),
                    DeviceId::new(13, 1),
                )
                .unwrap()
            ),
            Err(VfsError::AlreadyExists)
        ));
        assert_eq!(handle.remove(), Err(VfsError::NotFound));

        registry
            .finish_disconnect(handle.index, handle.generation)
            .unwrap();
        assert!(registry.device("virtual", "event0").is_none());
    }

    #[test]
    fn child_uevent_payload_uses_the_canonical_path_and_dev_identity() {
        let registration = DeviceRegistration::try_new(
            DeviceIdentity::new(
                "virtio0".into(),
                "input".into(),
                "event0".into(),
                DeviceId::new(13, 64),
            )
            .unwrap()
            .with_devname("input/event0".into())
            .unwrap()
            .child_of("virtio0".into(), "input0".into())
            .unwrap(),
            "input".into(),
            Vec::new(),
            None,
        )
        .unwrap();
        assert_eq!(
            registration.uevent_payload(),
            "MAJOR=13\nMINOR=64\nDEVNAME=input/event0\nDEVPATH=/devices/virtio0/input0/event0\\
             nSUBSYSTEM=input\nDEVTYPE=input\n",
        );
    }

    #[test]
    fn class_input_links_resolve_from_class_to_canonical_parent_and_subsystem() {
        // This mirrors `/sys/class/input/event0 -> ../../devices/.../event0`
        // and then resolves the links stored on its canonical kobject.
        let class_event = "/sys/class/input/event0";
        let canonical_event = resolve_from(class_event, "../../devices/virtio0/input0/event0");
        assert_eq!(canonical_event, "/sys/devices/virtio0/input0/event0");
        assert_eq!(
            resolve_from(
                &alloc::format!("{canonical_event}/device"),
                &device_link_target()
            ),
            "/sys/devices/virtio0/input0"
        );

        let identity = DeviceIdentity::new(
            "virtio0".into(),
            "input".into(),
            "event0".into(),
            DeviceId::new(13, 64),
        )
        .unwrap()
        .child_of("virtio0".into(), "input0".into())
        .unwrap();
        assert_eq!(
            resolve_from(
                &alloc::format!("{canonical_event}/subsystem"),
                &subsystem_link_target(&identity)
            ),
            "/sys/class/input"
        );
    }

    #[test]
    fn class_device_link_remains_generic_for_top_level_devices() {
        let class_card = "/sys/class/drm/card0";
        let canonical_card = resolve_from(class_card, "../../devices/virtio0/card0");
        assert_eq!(canonical_card, "/sys/devices/virtio0/card0");
        assert_eq!(
            resolve_from(
                &alloc::format!("{canonical_card}/device"),
                &device_link_target()
            ),
            "/sys/devices/virtio0"
        );
    }

    #[test]
    fn child_devices_are_not_resolvable_as_bus_root_devices() {
        let registry = DeviceRegistry::<2>::new();
        let parent =
            DeviceIdentity::without_dev("virtio0".into(), "input".into(), "input0".into()).unwrap();
        let child = DeviceIdentity::new(
            "virtio0".into(),
            "input".into(),
            "event0".into(),
            DeviceId::new(13, 64),
        )
        .unwrap()
        .child_of("virtio0".into(), "input0".into())
        .unwrap();
        let parent_registration =
            DeviceRegistration::try_new(parent.clone(), "input".into(), Vec::new(), None).unwrap();
        let child_registration =
            DeviceRegistration::try_new(child.clone(), "input".into(), Vec::new(), None).unwrap();
        let parent_reservation = registry.reserve(parent).unwrap();
        let child_reservation = registry.reserve(child).unwrap();
        let _ = DeviceReservation::publish_pair(
            parent_reservation,
            parent_registration,
            child_reservation,
            child_registration,
        )
        .unwrap();

        assert!(registry.root_device("virtio0", "input0").is_some());
        assert!(registry.root_device("virtio0", "event0").is_none());
    }

    #[test]
    fn devname_is_relative_and_is_not_the_sysfs_object_name() {
        let identity = DeviceIdentity::new(
            "virtio0".into(),
            "drm".into(),
            "card0".into(),
            DeviceId::new(226, 0),
        )
        .unwrap()
        .with_devname("dri/card0".into())
        .unwrap();
        assert_eq!(identity.name, "card0");
        assert_eq!(identity.devname.as_deref(), Some("dri/card0"));
        assert!(
            identity
                .clone()
                .with_devname("/dev/dri/card0".into())
                .is_err()
        );
        assert!(identity.with_devname("../card0".into()).is_err());
    }

    #[test]
    fn drm_uevent_uses_the_dri_devname() {
        let registration = DeviceRegistration::try_new(
            DeviceIdentity::new(
                "virtio0".into(),
                "drm".into(),
                "card0".into(),
                DeviceId::new(226, 0),
            )
            .unwrap()
            .with_devname("dri/card0".into())
            .unwrap(),
            "drm_minor".into(),
            Vec::new(),
            None,
        )
        .unwrap();
        assert!(
            registration
                .uevent_payload()
                .contains("DEVNAME=dri/card0\n")
        );
        assert!(!registration.uevent_payload().contains("DEVNAME=card0\n"));
    }

    #[test]
    fn bounded_many_publish_never_exposes_a_partial_set() {
        let registry = DeviceRegistry::<2>::new();
        let card = DeviceIdentity::new(
            "virtio0".into(),
            "drm".into(),
            "card0".into(),
            DeviceId::new(226, 0),
        )
        .unwrap();
        let connector =
            DeviceIdentity::without_dev("virtio0".into(), "drm".into(), "card0-Virtual-1".into())
                .unwrap();
        let render = DeviceIdentity::new(
            "virtio0".into(),
            "drm".into(),
            "renderD128".into(),
            DeviceId::new(226, 128),
        )
        .unwrap();
        let card_reservation = registry.reserve(card.clone()).unwrap();
        let connector_reservation = registry.reserve(connector.clone()).unwrap();
        assert!(matches!(
            registry.reserve(render),
            Err(VfsError::ResourceBusy)
        ));
        assert!(registry.device("virtio0", "card0").is_none());
        assert!(registry.device("virtio0", "card0-Virtual-1").is_none());
        drop((card_reservation, connector_reservation));
    }

    #[test]
    fn bounded_many_publish_exposes_all_drm_nodes_together() {
        let registry = DeviceRegistry::<3>::new();
        let identities = [
            DeviceIdentity::new(
                "virtio0".into(),
                "drm".into(),
                "card0".into(),
                DeviceId::new(226, 0),
            )
            .unwrap(),
            DeviceIdentity::without_dev("virtio0".into(), "drm".into(), "card0-Virtual-1".into())
                .unwrap(),
            DeviceIdentity::new(
                "virtio0".into(),
                "drm".into(),
                "renderD128".into(),
                DeviceId::new(226, 128),
            )
            .unwrap(),
        ];
        let registrations = identities.clone().map(|identity| {
            DeviceRegistration::try_new(identity, "drm_minor".into(), Vec::new(), None).unwrap()
        });
        let reservations = identities.map(|identity| registry.reserve(identity).unwrap());
        let [card_reservation, connector_reservation, render_reservation] = reservations;
        let _handles = DeviceReservation::publish_many([
            (card_reservation, registrations[0].clone()),
            (connector_reservation, registrations[1].clone()),
            (render_reservation, registrations[2].clone()),
        ])
        .unwrap();
        assert!(registry.device("virtio0", "card0").is_some());
        assert!(registry.device("virtio0", "card0-Virtual-1").is_some());
        assert!(registry.device("virtio0", "renderD128").is_some());
    }
}
