//! Truthful sysfs description of the PMUs committed by the x86 PMU fleet.
//!
//! This is deliberately a description, not an event translation layer.  In
//! particular, raw encodings are not advertised here: the perf open planner
//! still rejects them until it can bind the exact encoding to a core type.

use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PmuKind {
    Cpu,
    CpuCore,
    CpuAtom,
    IntelPt,
    IntelBts,
    /// Software probe PMUs are always present: they are dynamic perf type
    /// numbers, never Linux's reserved PERF_TYPE_KPROBE/UPROBE constants.
    Kprobe,
    Uprobe,
    /// A platform-discovered, package-scoped PMU.  Its name and dynamic type
    /// are immutable for this boot and are never synthesized for unknown
    /// hardware.
    Discovered {
        box_type: u16,
        box_id: u16,
        type_number: u32,
    },
    Fixed(&'static str, u32),
}

impl PmuKind {
    pub(crate) fn name(self) -> String {
        match self {
            Self::Cpu => String::from("cpu"),
            Self::CpuCore => String::from("cpu_core"),
            Self::CpuAtom => String::from("cpu_atom"),
            Self::IntelPt => String::from("intel_pt"),
            Self::IntelBts => String::from("intel_bts"),
            Self::Kprobe => String::from("kprobe"),
            Self::Uprobe => String::from("uprobe"),
            Self::Discovered {
                box_type, box_id, ..
            } => {
                format!("uncore_type_{box_type}_{box_id}")
            }
            Self::Fixed(name, _) => String::from(name),
        }
    }

    const fn type_number(self) -> u32 {
        // Linux allocates PMU type numbers dynamically.  TheKernel keeps
        // these stable only within one boot, which is the UAPI contract.
        match self {
            Self::Cpu => 4,
            Self::CpuCore => 16,
            Self::CpuAtom => 17,
            Self::IntelPt => 18,
            Self::IntelBts => 19,
            Self::Kprobe => 20,
            Self::Uprobe => 21,
            Self::Discovered { type_number, .. } => type_number,
            Self::Fixed(_, type_number) => type_number,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PmuDescription {
    pub(crate) kind: PmuKind,
    pub(crate) cpus: String,
    /// Package-owner identity for sources whose metadata must be obtained
    /// from a package-scoped hardware register.
    pub(crate) owner_cpu: Option<usize>,
    pub(crate) identifier: String,
    pub(crate) max_precise: u8,
    pub(crate) events: PmuEvents,
    pub(crate) format: &'static [(&'static str, &'static str)],
    pub(crate) caps: &'static [(&'static str, &'static str)],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PmuEvents {
    Architectural,
    Fixed(&'static [(&'static str, &'static str)]),
    None,
}

/// The dynamic type namespace is boot-local, just like Linux's.  Resolve it
/// from the same committed descriptor source that sysfs uses; open must never
/// infer an uncore box from an arbitrary number supplied by userspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DynamicPmu {
    CpuCore,
    CpuAtom,
    IntelPt,
    IntelBts,
    Uncore { box_type: u16, box_id: u16 },
    ReadOnly(axhal::perf_uncore::ReadOnlyPmu),
    Kprobe,
    Uprobe,
}

#[cfg(feature = "pmu")]
pub(crate) fn dynamic_pmu(type_number: u32) -> Option<DynamicPmu> {
    for description in registered_pmus() {
        let kind = description.kind;
        if kind.type_number() != type_number {
            continue;
        }
        return match kind {
            PmuKind::CpuCore => Some(DynamicPmu::CpuCore),
            PmuKind::CpuAtom => Some(DynamicPmu::CpuAtom),
            PmuKind::IntelPt => Some(DynamicPmu::IntelPt),
            PmuKind::IntelBts => Some(DynamicPmu::IntelBts),
            PmuKind::Kprobe => Some(DynamicPmu::Kprobe),
            PmuKind::Uprobe => Some(DynamicPmu::Uprobe),
            PmuKind::Discovered {
                box_type, box_id, ..
            } => Some(DynamicPmu::Uncore { box_type, box_id }),
            PmuKind::Fixed(name, _) => axhal::perf_uncore::readonly_pmus()
                .find(|source| source.name == name)
                .map(|source| DynamicPmu::ReadOnly(source.pmu)),
            PmuKind::Cpu => None,
        };
    }
    None
}

#[cfg(not(feature = "pmu"))]
pub(crate) fn dynamic_pmu(type_number: u32) -> Option<DynamicPmu> {
    match type_number {
        20 => Some(DynamicPmu::Kprobe),
        21 => Some(DynamicPmu::Uprobe),
        _ => None,
    }
}

impl PmuDescription {
    pub(crate) fn type_file(&self) -> String {
        format!("{}\n", self.kind.type_number())
    }

    /// Linux perf reads energy metadata directly from the PMU directory.
    /// It is deliberately generated from the owner CPU's immutable RAPL
    /// unit MSR rather than guessed from a Panther Lake model table.
    pub(crate) fn event_metadata_for(&self, event: &str) -> Option<(String, String)> {
        let owner_cpu = power_metadata_owner(self.kind, event, self.owner_cpu)?;
        let power_unit = axhal::perf_uncore::rapl_power_unit_for_owner(owner_cpu).ok()?;
        Some((
            rapl_energy_scale_decimal(power_unit)?,
            String::from("Joules\n"),
        ))
    }
}

/// Resolve the immutable RAPL-unit read target from the already published PMU
/// descriptor.  Keeping this separate from the hardware access makes it
/// impossible for sysfs mount CPU affinity to select a different package
/// owner.
fn power_metadata_owner(
    kind: PmuKind,
    event: &str,
    owner_cpu: Option<usize>,
) -> Option<usize> {
    match kind {
        PmuKind::Fixed("power", _) if power_event_has_metadata(event) => owner_cpu,
        _ => None,
    }
}

fn power_event_has_metadata(event: &str) -> bool {
    matches!(event, "energy-pkg" | "energy-cores")
}

/// Render Intel RAPL's exact `2^-ENERGY_UNIT` Joule multiplier without
/// floating point.  `5^N / 10^N` produces a finite decimal for the five-bit
/// ENERGY_UNIT field and avoids incorrectly publishing `10^-N`.
pub(crate) fn rapl_energy_scale_decimal(power_unit: u64) -> Option<String> {
    axhal::perf_uncore::rapl_energy_unit_q32(power_unit)?;
    let exponent = ((power_unit >> 8) & 0x1f) as usize;
    if exponent == 0 {
        return Some(String::from("1\n"));
    }
    let numerator = 5u128.checked_pow(exponent as u32)?;
    Some(format!("0.{numerator:0width$}\n", width = exponent))
}

/// Return only PMUs for which the platform fleet has committed a capability
/// record.  Panther Lake core PMUs publish one exact PEBS level only after
/// the fleet/product gate above has accepted the hardware.
#[cfg(feature = "pmu")]
pub(crate) fn registered_pmus() -> Vec<PmuDescription> {
    use axhal::pmu::{IntelCoreType, ProductClass};

    let Ok(cpu_count) = axhal::pmu::fleet_cpu_count() else {
        return probe_pmus(axhal::cpu_num().min(axconfig::plat::MAX_CPU_NUM));
    };
    let mut snapshots = Vec::new();
    if snapshots.try_reserve_exact(cpu_count).is_err() {
        return probe_pmus(cpu_count);
    }
    for cpu in 0..cpu_count {
        let Ok(snapshot) = axhal::pmu::fleet_capability_snapshot(cpu) else {
            return probe_pmus(cpu_count);
        };
        snapshots.push(snapshot);
    }
    let Some(first) = snapshots.first().copied() else {
        return probe_pmus(cpu_count);
    };
    let identifier = format!(
        "intel-family-{}-model-{:x}-pmu-v{}",
        first.family, first.model, first.capabilities.version
    );
    let panther_hybrid = snapshots.iter().all(|snapshot| {
        snapshot.product == ProductClass::PantherLake
            && snapshot.family == 6
            && snapshot.model == 0xcc
            && matches!(
                snapshot.core_type,
                IntelCoreType::Core | IntelCoreType::Atom
            )
    });
    if !panther_hybrid {
        // Intel PMU v4+ outside the explicitly accepted Panther Lake hybrid
        // product gets only its architectural CPU PMU identity.
        let mut registered = vec![PmuDescription {
            kind: PmuKind::Cpu,
            cpus: cpu_list(0..cpu_count),
            owner_cpu: None,
            identifier,
            max_precise: 0,
            events: PmuEvents::Architectural,
            format: &PMU_FORMAT,
            caps: &[],
        }];
        registered.extend(probe_pmus(cpu_count));
        return registered;
    }
    let core_cpus: Vec<usize> = snapshots
        .iter()
        .enumerate()
        .filter_map(|(cpu, snapshot)| (snapshot.core_type == IntelCoreType::Core).then_some(cpu))
        .collect();
    let atom_cpus: Vec<usize> = snapshots
        .iter()
        .enumerate()
        .filter_map(|(cpu, snapshot)| (snapshot.core_type == IntelCoreType::Atom).then_some(cpu))
        .collect();
    if core_cpus.is_empty() || atom_cpus.is_empty() {
        return probe_pmus(cpu_count);
    }
    let mut registered = vec![
        PmuDescription {
            kind: PmuKind::CpuCore,
            cpus: cpu_list(core_cpus),
            owner_cpu: None,
            identifier: identifier.clone(),
            max_precise: 1,
            events: PmuEvents::Fixed(&CPU_CORE_TYPED_EVENTS),
            format: &PMU_FORMAT,
            caps: &[],
        },
        PmuDescription {
            kind: PmuKind::CpuAtom,
            cpus: cpu_list(atom_cpus),
            owner_cpu: None,
            identifier,
            max_precise: 0,
            events: PmuEvents::Fixed(&CPU_ATOM_TYPED_EVENTS),
            format: &PMU_FORMAT,
            caps: &[],
        },
    ];
    // Uncore/energy/residency sources have no architectural MSR layout.  The
    // platform returns an empty iterator unless an Intel PerfMon-Discovery
    // decoder supplied bounded register records for the committed single
    // Panther Lake package.
    for (index, source) in axhal::perf_uncore::discovered_pmus().enumerate() {
        let type_number = 32_u32.saturating_add(index as u32);
        registered.push(PmuDescription {
            kind: PmuKind::Discovered {
                box_type: source.box_type,
                box_id: source.box_id,
                type_number,
            },
            cpus: format!("{}\n", source.cpus),
            owner_cpu: Some(source.cpus),
            identifier: format!(
                "intel-panther-lake-package-{}-{}-{}bit",
                source.package_id, source.box_type, source.width
            ),
            max_precise: 0,
            events: PmuEvents::None,
            format: &PMU_FORMAT,
            caps: &[],
        });
    }
    for (index, source) in axhal::perf_uncore::readonly_pmus().enumerate() {
        let events: &'static [(&'static str, &'static str)] = match source.pmu {
            axhal::perf_uncore::ReadOnlyPmu::Msr => &MSR_EVENTS,
            axhal::perf_uncore::ReadOnlyPmu::Power => &POWER_EVENTS,
            axhal::perf_uncore::ReadOnlyPmu::CoreCstate => &CORE_CSTATE_EVENTS,
            axhal::perf_uncore::ReadOnlyPmu::PackageCstate => &PACKAGE_CSTATE_EVENTS,
        };
        registered.push(PmuDescription {
            kind: PmuKind::Fixed(source.name, 64_u32.saturating_add(index as u32)),
            cpus: if source.package_scoped {
                format!("{}\n", source.owner_cpu)
            } else {
                cpu_list(0..cpu_count)
            },
            owner_cpu: source.package_scoped.then_some(source.owner_cpu),
            identifier: format!(
                "intel-panther-lake-package-{}-{}",
                source.package_id, source.name
            ),
            max_precise: 0,
            events: PmuEvents::Fixed(events),
            format: &PMU_FORMAT,
            caps: &[],
        });
    }
    // PT and BTS are transports rather than generic counters.  Discover the
    // actual CPUID-backed backend and publish exactly that one, with its
    // native config fields and capabilities.  In particular BTS is not
    // advertised on a PT-capable machine merely because Debug Store exists.
    #[cfg(target_os = "none")]
    if let Ok(backend) = axhal::perf_precise_aux::discover_aux_backend() {
        let (kind, identifier, events, format, caps) = match backend {
            axhal::perf_precise_aux::AuxBackend::IntelPt => (
                PmuKind::IntelPt,
                String::from("intel-panther-lake-pt"),
                PmuEvents::None,
                &INTEL_PT_FORMAT[..],
                &INTEL_PT_CAPS[..],
            ),
            axhal::perf_precise_aux::AuxBackend::Bts => (
                PmuKind::IntelBts,
                String::from("intel-panther-lake-bts"),
                PmuEvents::None,
                &INTEL_BTS_FORMAT[..],
                &INTEL_BTS_CAPS[..],
            ),
        };
        registered.push(PmuDescription {
            kind,
            cpus: cpu_list(0..cpu_count),
            owner_cpu: None,
            identifier,
            max_precise: 0,
            events,
            format,
            caps,
        });
    }
    registered.extend(probe_pmus(cpu_count));
    registered
}

fn probe_pmus(cpu_count: usize) -> Vec<PmuDescription> {
    let cpus = cpu_list(0..cpu_count.max(1));
    vec![
        PmuDescription {
            kind: PmuKind::Kprobe,
            cpus: cpus.clone(),
            owner_cpu: None,
            identifier: String::from("thekernel-kprobe"),
            max_precise: 0,
            events: PmuEvents::None,
            format: &PROBE_FORMAT,
            caps: &PROBE_CAPS,
        },
        PmuDescription {
            kind: PmuKind::Uprobe,
            cpus,
            owner_cpu: None,
            identifier: String::from("thekernel-uprobe"),
            max_precise: 0,
            events: PmuEvents::None,
            format: &PROBE_FORMAT,
            caps: &PROBE_CAPS,
        },
    ]
}

fn cpu_list(cpus: impl IntoIterator<Item = usize>) -> String {
    let mut result = String::new();
    for (index, cpu) in cpus.into_iter().enumerate() {
        if index != 0 {
            result.push(',');
        }
        result.push_str(&cpu.to_string());
    }
    result.push('\n');
    result
}

#[cfg(not(feature = "pmu"))]
pub(crate) fn registered_pmus() -> Vec<PmuDescription> {
    probe_pmus(axhal::cpu_num().min(axconfig::plat::MAX_CPU_NUM))
}

pub(crate) const PMU_FORMAT: [(&str, &str); 5] = [
    ("event", "config:0-7\n"),
    ("umask", "config:8-15\n"),
    ("edge", "config:18\n"),
    ("inv", "config:23\n"),
    ("thresh", "config:24-31\n"),
];
pub(crate) const INTEL_PT_FORMAT: [(&str, &str); 10] = [
    ("pt", "config:0\n"),
    ("cyc", "config:1\n"),
    ("mtc", "config:9\n"),
    ("tsc", "config:10\n"),
    ("noretcomp", "config:11\n"),
    ("ptw", "config:12\n"),
    ("branch", "config:13\n"),
    ("mtc_period", "config:14-17\n"),
    ("cyc_thresh", "config:19-22\n"),
    ("psb_period", "config:24-27\n"),
];
pub(crate) const INTEL_BTS_FORMAT: [(&str, &str); 0] = [];
pub(crate) const PROBE_FORMAT: [(&str, &str); 3] = [
    ("config", "config:0-63\n"),
    ("func", "config1:0-63\n"),
    ("offset", "config2:0-63\n"),
];
pub(crate) const PROBE_CAPS: [(&str, &str); 1] = [("retprobe", "1\n")];
pub(crate) const INTEL_PT_CAPS: [(&str, &str); 2] = [("aux-output", "1\n"), ("snapshot", "1\n")];
pub(crate) const INTEL_BTS_CAPS: [(&str, &str); 1] = [("aux-output", "1\n")];

/// Architectural event aliases have identical semantics on Intel Core and
/// Atom.  All non-architectural/raw encodings remain rejected by the open
/// path until type-specific support exists.
pub(crate) const PMU_EVENTS: [(&str, &str); 2] = [
    ("cpu-cycles", "event=0x3c\n"),
    ("instructions", "event=0xc0\n"),
];

/// Type-local encodings are published only under the matching hybrid PMU.
/// They are architectural encodings whose CPUID availability is checked at
/// open/placement; generic `cpu` deliberately never exposes this list.
pub(crate) const CPU_CORE_TYPED_EVENTS: [(&str, &str); 8] = [
    ("cache-references", "event=0x2e,umask=0x4f\n"),
    ("cache-misses", "event=0x2e,umask=0x41\n"),
    ("branches", "event=0xc4\n"),
    ("branch-misses", "event=0xc5\n"),
    ("bus-cycles", "event=0x3c,umask=0x01\n"),
    ("ref-cycles", "event=0x3c,umask=0x01\n"),
    ("stalled-cycles-frontend", "event=0xa3,umask=0x01\n"),
    ("stalled-cycles-backend", "event=0xa3,umask=0x02\n"),
];
pub(crate) const CPU_ATOM_TYPED_EVENTS: [(&str, &str); 6] = [
    ("cache-references", "event=0x2e,umask=0x4f\n"),
    ("cache-misses", "event=0x2e,umask=0x41\n"),
    ("branches", "event=0xc4\n"),
    ("branch-misses", "event=0xc5\n"),
    ("bus-cycles", "event=0x3c,umask=0x01\n"),
    ("ref-cycles", "event=0x3c,umask=0x01\n"),
];

pub(crate) const MSR_EVENTS: [(&str, &str); 3] = [
    ("aperf", "event=0x0\n"),
    ("mperf", "event=0x1\n"),
    ("tsc", "event=0x2\n"),
];
pub(crate) const POWER_EVENTS: [(&str, &str); 2] = [
    ("energy-pkg", "event=0x0\n"),
    ("energy-cores", "event=0x1\n"),
];
pub(crate) const CORE_CSTATE_EVENTS: [(&str, &str); 3] = [
    ("c3-residency", "event=0x0\n"),
    ("c6-residency", "event=0x1\n"),
    ("c7-residency", "event=0x2\n"),
];
pub(crate) const PACKAGE_CSTATE_EVENTS: [(&str, &str); 7] = [
    ("c2-residency", "event=0x0\n"),
    ("c3-residency", "event=0x1\n"),
    ("c6-residency", "event=0x2\n"),
    ("c7-residency", "event=0x3\n"),
    ("c8-residency", "event=0x4\n"),
    ("c9-residency", "event=0x5\n"),
    ("c10-residency", "event=0x6\n"),
];

#[cfg(test)]
mod tests {
    use super::{
        PMU_EVENTS, PMU_FORMAT, PmuKind, power_event_has_metadata, power_metadata_owner,
        rapl_energy_scale_decimal,
    };

    #[test]
    fn registry_exports_only_architectural_shared_aliases() {
        assert_eq!(PmuKind::Cpu.name(), "cpu");
        assert_eq!(PmuKind::CpuCore.name(), "cpu_core");
        assert_eq!(PmuKind::CpuAtom.name(), "cpu_atom");
        assert_eq!(
            PMU_EVENTS,
            [
                ("cpu-cycles", "event=0x3c\n"),
                ("instructions", "event=0xc0\n")
            ]
        );
        assert_eq!(PMU_FORMAT[0], ("event", "config:0-7\n"));
    }

    #[test]
    fn rapl_scale_is_exact_binary_energy_unit_in_decimal() {
        assert_eq!(rapl_energy_scale_decimal(0), Some(String::from("1\n")));
        assert_eq!(
            rapl_energy_scale_decimal(14 << 8),
            Some(String::from("0.00006103515625\n"))
        );
        assert_eq!(
            rapl_energy_scale_decimal(31 << 8),
            Some(String::from("0.0000000004656612873077392578125\n"))
        );
    }

    #[test]
    fn power_metadata_is_only_published_as_energy_event_siblings() {
        assert!(power_event_has_metadata("energy-pkg"));
        assert!(power_event_has_metadata("energy-cores"));
        assert!(!power_event_has_metadata("aperf"));
        assert!(!power_event_has_metadata("power.scale"));
    }

    #[test]
    fn power_metadata_uses_the_published_package_owner() {
        let power = PmuKind::Fixed("power", 65);
        assert_eq!(power_metadata_owner(power, "energy-pkg", Some(3)), Some(3));
        assert_eq!(
            power_metadata_owner(power, "energy-cores", Some(3)),
            Some(3)
        );
        assert_eq!(power_metadata_owner(power, "energy-pkg", None), None);
        assert_eq!(power_metadata_owner(power, "aperf", Some(3)), None);
        assert_eq!(
            power_metadata_owner(PmuKind::Fixed("msr", 64), "energy-pkg", Some(3)),
            None
        );
    }
}
