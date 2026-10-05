//! Linux-style CPU idle attributes. No policy is enabled by mounting sysfs.
use alloc::{format, string::String, sync::Arc};

use axfs_ng_vfs::{VfsError, VfsResult};
use axhal::cpu_power::cpuidle;

use super::{DirMapping, RwFile, SimpleDir, SimpleFile, SimpleFileOperation, SimpleFs};
pub fn register(fs: &Arc<SimpleFs>, root: &mut DirMapping) {
    let mut global = DirMapping::new();
    global.add(
        "current_driver",
        SimpleFile::new_regular(fs.clone(), || {
            Ok(if cpuidle::enabled() && cpuidle::supported(0) {
                "intel_idle\n"
            } else {
                "halt\n"
            })
        }),
    );
    global.add(
        "current_governor_ro",
        SimpleFile::new_regular(fs.clone(), || Ok("thekernel_timer\n")),
    );
    global.add(
        "mwait_enabled",
        SimpleFile::new_regular(fs.clone(), || {
            Ok(format!("{}\n", u8::from(cpuidle::enabled())))
        }),
    );
    root.add(
        "cpuidle",
        SimpleDir::new_maker(fs.clone(), Arc::new(global)),
    );
    for cpu in 0..axhal::cpu_num() {
        let mut cpu_dir = DirMapping::new();
        let mut idle = DirMapping::new();
        for index in 0..cpuidle::state_count(cpu) {
            let state = cpuidle::state(cpu, index).unwrap();
            let mut attrs = DirMapping::new();
            for (name, value) in [
                ("name", format!("{}\n", state.name)),
                ("desc", format!("{}\n", state.desc)),
                ("latency", format!("{}\n", state.latency)),
                ("residency", format!("{}\n", state.residency)),
            ] {
                attrs.add(
                    name,
                    SimpleFile::new_regular(fs.clone(), move || Ok(value.clone())),
                );
            }
            attrs.add(
                "usage",
                SimpleFile::new_regular(fs.clone(), move || {
                    Ok(format!("{}\n", cpuidle::counters(cpu, index).unwrap().0))
                }),
            );
            attrs.add(
                "time",
                SimpleFile::new_regular(fs.clone(), move || {
                    Ok(format!("{}\n", cpuidle::counters(cpu, index).unwrap().1))
                }),
            );
            attrs.add(
                "disable",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |op| -> VfsResult<Option<String>> {
                        match op {
                            SimpleFileOperation::Read => Ok(Some(format!(
                                "{}\n",
                                u8::from(cpuidle::disabled(cpu, index).unwrap())
                            ))),
                            SimpleFileOperation::Write(data) => {
                                cpuidle::set_disabled(cpu, index, parse_disable(data)?)
                                    .map_err(|_| VfsError::InvalidInput)?;
                                Ok(None)
                            }
                        }
                    }),
                ),
            );
            idle.add(
                format!("state{index}"),
                SimpleDir::new_maker(fs.clone(), Arc::new(attrs)),
            );
        }
        idle.add(
            "mwait_supported",
            SimpleFile::new_regular(fs.clone(), move || {
                Ok(format!("{}\n", u8::from(cpuidle::supported(cpu))))
            }),
        );
        cpu_dir.add("cpuidle", SimpleDir::new_maker(fs.clone(), Arc::new(idle)));
        cpu_dir.add("online", SimpleFile::new_regular(fs.clone(), || Ok("1\n")));
        super::cpu_frequency::register(cpu, fs, &mut cpu_dir);
        root.add(
            format!("cpu{cpu}"),
            SimpleDir::new_maker(fs.clone(), Arc::new(cpu_dir)),
        );
    }
}
fn parse_disable(data: &[u8]) -> VfsResult<bool> {
    match core::str::from_utf8(data)
        .map_err(|_| VfsError::InvalidInput)?
        .trim()
    {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(VfsError::InvalidInput),
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn disable_values_are_boolean_only() {
        assert_eq!(super::parse_disable(b"1\n").unwrap(), true);
        assert_eq!(super::parse_disable(b"0").unwrap(), false);
        for v in [b"2".as_slice(), b"-1", b"", b"0 1"] {
            assert!(super::parse_disable(v).is_err());
        }
    }
}
