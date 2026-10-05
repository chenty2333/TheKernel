//! Real cached USB devices; no synthetic root-hub vendor identity.
#[cfg(feature = "input")]
mod enabled {
    use alloc::{format, vec::Vec};

    use axfs_ng_vfs::{VfsError, VfsResult};
    use spin::Mutex;

    use crate::pseudofs::device_registry::{
        DeviceAttribute, DeviceHandle, DeviceIdentity, DeviceRegistration, global_device_registry,
    };

    struct Published {
        initialized: bool,
        handles: Vec<DeviceHandle<'static, 64>>,
    }
    static PUBLISHED: Mutex<Published> = Mutex::new(Published {
        initialized: false,
        handles: Vec::new(),
    });

    pub(super) fn publish_inventory() {
        let mut published = PUBLISHED.lock();
        if published.initialized {
            return;
        }
        let observations = match axdriver::usb_observations::snapshot() {
            Ok(observations) => observations,
            Err(error) => {
                warn!("USB sysfs observations unavailable: {error:?}");
                return;
            }
        };
        for observation in observations {
            let result = (|| -> VfsResult<()> {
                let name = axdriver::usb_observations::name(&observation);
                let identity = DeviceIdentity::without_dev(
                    format!("usb{}", observation.bus),
                    "usb".into(),
                    name,
                )?;
                let registry = global_device_registry();
                let reservation = registry.reserve(identity.clone())?;
                let mut attributes = Vec::new();
                attributes
                    .try_reserve_exact(10)
                    .map_err(|_| VfsError::NoMemory)?;
                let bus = observation.bus;
                let address = observation.location.address;
                attributes.push(DeviceAttribute::try_new("busnum".into(), move || {
                    Ok(format!("{bus}\n"))
                })?);
                attributes.push(DeviceAttribute::try_new("devnum".into(), move || {
                    Ok(format!("{address}\n"))
                })?);
                let speed = axdriver::usb_observations::speed(&observation);
                let desc = observation.descriptor;
                let vendor = desc.vendor_id;
                let product = desc.product_id;
                attributes.push(DeviceAttribute::try_new("idVendor".into(), move || {
                    Ok(format!("{vendor:04x}\n"))
                })?);
                attributes.push(DeviceAttribute::try_new("idProduct".into(), move || {
                    Ok(format!("{product:04x}\n"))
                })?);
                let class = desc.class;
                attributes.push(DeviceAttribute::try_new(
                    "bDeviceClass".into(),
                    move || Ok(format!("{class:02x}\n")),
                )?);
                let configurations = desc.num_configurations;
                attributes.push(DeviceAttribute::try_new(
                    "bNumConfigurations".into(),
                    move || Ok(format!("{configurations}\n")),
                )?);
                if let Some(configuration) = observation.location.configuration {
                    attributes.push(DeviceAttribute::try_new(
                        "bConfigurationValue".into(),
                        move || Ok(format!("{configuration}\n")),
                    )?);
                }
                if let Some(speed) = speed {
                    attributes.push(DeviceAttribute::try_new("speed".into(), move || Ok(speed))?);
                }
                let bytes = observation.descriptors;
                attributes.push(DeviceAttribute::try_new("descriptors".into(), move || {
                    Ok(bytes.as_ref().clone())
                })?);
                published
                    .handles
                    .try_reserve(1)
                    .map_err(|_| VfsError::NoMemory)?;
                let device = DeviceRegistration::try_bus_device(
                    identity,
                    "usb_device".into(),
                    attributes,
                    "usb".into(),
                    false,
                )?;
                published.handles.push(reservation.publish(device)?);
                Ok(())
            })();
            if let Err(error) = result {
                warn!("USB sysfs publication failed: {error:?}");
                break;
            }
        }
        published.initialized = true;
    }
}

#[cfg(feature = "input")]
pub(super) fn publish_inventory() {
    enabled::publish_inventory();
}

#[cfg(not(feature = "input"))]
pub(super) fn publish_inventory() {}
