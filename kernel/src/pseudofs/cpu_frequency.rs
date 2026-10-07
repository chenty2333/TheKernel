//! HWP-only cpufreq nodes; absent hardware never masquerades as intel_pstate.
use alloc::{format, string::String, sync::Arc};

use axfs_ng_vfs::{VfsError, VfsResult};
use axhal::cpu_power::cpufreq;

use super::{DirMapping, RwFile, SimpleDir, SimpleFile, SimpleFileOperation, SimpleFs};
fn preference(epp: u8) -> String {
    match epp {
        0 => "performance".into(),
        128 => "balance_performance".into(),
        192 => "balance_power".into(),
        255 => "power".into(),
        value => format!("{value}"),
    }
}
pub fn register(cpu: usize, fs: &Arc<SimpleFs>, root: &mut DirMapping) {
    root.add(
        "cpufreq_supported",
        SimpleFile::new_regular(fs.clone(), move || {
            Ok(format!("{}\n", u8::from(cpufreq::snapshot(cpu).is_some())))
        }),
    );
    let Some(initial) = cpufreq::snapshot(cpu) else {
        return;
    };
    let mut dir = DirMapping::new();
    for (name, value) in [
        ("scaling_driver", "intel_pstate\n"),
        ("scaling_available_governors", "performance powersave\n"),
    ] {
        dir.add(name, SimpleFile::new_regular(fs.clone(), move || Ok(value)));
    }
    for (name, value) in [
        (
            "cpuinfo_min_freq",
            initial.caps.frequency(initial.caps.lowest),
        ),
        (
            "cpuinfo_max_freq",
            initial.caps.frequency(initial.caps.highest),
        ),
    ] {
        dir.add(
            name,
            SimpleFile::new_regular(fs.clone(), move || Ok(format!("{value}\n"))),
        );
    }
    dir.add(
        "scaling_cur_freq",
        SimpleFile::new_regular(fs.clone(), move || {
            cpufreq::snapshot(cpu)
                .and_then(|p| p.current_khz)
                .map(|khz| format!("{khz}\n"))
                .ok_or(VfsError::InvalidData)
        }),
    );
    for field in [
        "scaling_governor",
        "scaling_min_freq",
        "scaling_max_freq",
        "energy_performance_preference",
    ] {
        if field == "energy_performance_preference" && initial.epp.is_none() {
            continue;
        }
        dir.add(
            field,
            SimpleFile::new_regular(
                fs.clone(),
                RwFile::new(move |op| -> VfsResult<Option<String>> {
                    match op {
                        SimpleFileOperation::Read => {
                            let p = cpufreq::snapshot(cpu).ok_or(VfsError::OperationNotSupported)?;
                            Ok(Some(format!(
                                "{}\n",
                                match field {
                                    "scaling_governor" =>
                                        if p.performance {
                                            "performance".into()
                                        } else {
                                            "powersave".into()
                                        },
                                    "scaling_min_freq" => format!("{}", p.caps.frequency(p.min)),
                                    "scaling_max_freq" => format!("{}", p.caps.frequency(p.max)),
                                    _ => preference(p.epp.unwrap()),
                                }
                            )))
                        }
                        SimpleFileOperation::Write(data) => {
                            let value =
                                core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                            cpufreq::update(cpu, field, value).map_err(|e| match e {
                                cpufreq::Error::Unsupported => VfsError::OperationNotSupported,
                                cpufreq::Error::InvalidInput => VfsError::InvalidInput,
                            })?;
                            #[cfg(feature = "hwp-uclamp")]
                            axtask::refresh_cpu_power_policy(cpu);
                            Ok(None)
                        }
                    }
                }),
            ),
        );
    }
    if initial.epp.is_some() {
        dir.add(
            "energy_performance_available_preferences",
            SimpleFile::new_regular(fs.clone(), || {
                Ok("default performance balance_performance balance_power power\n")
            }),
        );
    }
    root.add("cpufreq", SimpleDir::new_maker(fs.clone(), Arc::new(dir)));
}
#[cfg(test)]
mod tests {
    #[test]
    fn preference_names_match_intel_pstate_values() {
        assert_eq!(super::preference(0), "performance");
        assert_eq!(super::preference(128), "balance_performance");
        assert_eq!(super::preference(192), "balance_power");
        assert_eq!(super::preference(255), "power");
        assert_eq!(super::preference(64), "64");
    }
}
