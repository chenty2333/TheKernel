//! Coretemp hwmon objects, with an empty hwmon class when unsupported.
use alloc::{collections::BTreeSet, format, sync::Arc, vec::Vec};

use axfs_ng_vfs::{VfsError, VfsResult};
use axhal::cpu_power::thermal;

use super::{
    DirMapping, SimpleDir, SimpleFs,
    device_registry::{self, DeviceAttribute, DeviceIdentity, DeviceRegistration},
};
fn add_sensor(
    attrs: &mut Vec<DeviceAttribute>,
    index: usize,
    sensor: thermal::Sensor,
    package: bool,
) -> VfsResult<()> {
    let cpu = sensor.cpu;
    let label = if package {
        format!("Package id {}\n", sensor.package)
    } else {
        format!("Core {}\n", sensor.core)
    };
    attrs.push(DeviceAttribute::try_new(
        format!("temp{index}_label"),
        move || Ok(label.clone()),
    )?);
    attrs.push(DeviceAttribute::try_new(
        format!("temp{index}_input"),
        move || {
            let sensor = thermal::snapshot(cpu).ok_or(VfsError::Io)?;
            let status = if package {
                sensor.package_status
            } else {
                sensor.core_status
            };
            thermal::temperature(status, sensor.target)
                .map(|mc| format!("{mc}\n"))
                .ok_or(VfsError::InvalidData)
        },
    )?);
    attrs.push(DeviceAttribute::try_new(
        format!("temp{index}_crit"),
        move || Ok(format!("{}\n", sensor.target.critical_mc)),
    )?);
    if let Some(max) = sensor.target.maximum_mc {
        attrs.push(DeviceAttribute::try_new(
            format!("temp{index}_max"),
            move || Ok(format!("{max}\n")),
        )?);
    }
    attrs.push(DeviceAttribute::try_new(
        format!("temp{index}_crit_alarm"),
        move || {
            let sensor = thermal::snapshot(cpu).ok_or(VfsError::Io)?;
            let status = if package {
                sensor.package_status
            } else {
                sensor.core_status
            };
            Ok(format!("{}\n", u8::from(status & (1 << 5) != 0)))
        },
    )?);
    Ok(())
}
fn register() -> VfsResult<()> {
    let sensors: Vec<_> = (0..axhal::cpu_num())
        .filter_map(thermal::snapshot)
        .collect();
    let mut packages = BTreeSet::new();
    for (hwmon, sensor) in sensors
        .iter()
        .filter(|s| packages.insert(s.package))
        .enumerate()
    {
        let registry = device_registry::global_device_registry();
        let identity = DeviceIdentity::without_dev(
            "platform".into(),
            "hwmon".into(),
            format!("hwmon{hwmon}"),
        )?
        .child_of("platform".into(), format!("coretemp.{}", sensor.package))?;
        let reservation = match registry.reserve(identity.clone()) {
            Ok(reservation) => reservation,
            Err(error) if error == VfsError::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let mut attrs = Vec::new();
        attrs.push(DeviceAttribute::try_new("name".into(), || {
            Ok("coretemp\n")
        })?);
        let mut index = 1;
        if sensor.package_available {
            add_sensor(&mut attrs, index, *sensor, true)?;
            index += 1;
        }
        let mut cores = BTreeSet::new();
        for core in sensors
            .iter()
            .filter(|s| s.package == sensor.package && cores.insert(s.core))
        {
            add_sensor(&mut attrs, index, *core, false)?;
            index += 1;
        }
        let device = DeviceRegistration::try_new(identity, "hwmon".into(), attrs, None)?;
        let _handle = reservation.publish(device)?;
    }
    Ok(())
}
pub(super) fn class_root(fs: Arc<SimpleFs>) -> DirMapping {
    if let Err(error) = register() {
        warn!("coretemp sysfs registration failed: {error:?}");
    }
    let mut extra = DirMapping::new();
    if !(0..axhal::cpu_num()).any(|cpu| thermal::snapshot(cpu).is_some()) {
        extra.add(
            "hwmon",
            SimpleDir::new_maker(fs.clone(), Arc::new(DirMapping::new())),
        );
    }
    extra
}
#[cfg(test)]
mod tests {
    use super::{super::SimpleFileOps, *};
    #[test]
    fn readonly_attributes_and_empty_unsupported_class() {
        let ops = || Ok("coretemp\n");
        assert_eq!(ops.default_permission().bits(), 0o444);
        assert!(ops.write_all(b"1").is_err());
        assert!(thermal::snapshot(0).is_none());
    }
}
