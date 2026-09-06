use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use axerrno::{AxError, AxResult};
use axtask::current;
use linux_perf::{
    PERF_ATTR_MAX_SIZE, PERF_ATTR_SIZE_OFFSET, PERF_ATTR_SIZE_VER0, PERF_FLAG_FD_NO_GROUP,
    PERF_FLAG_FD_OUTPUT, PERF_FLAG_PID_CGROUP, PerfAttrInput, PerfCapabilities, PerfEventAttr,
    PerfEventAttrV0, PerfOpenTarget, PerfTarget,
};
use thekernel_linux_perf as linux_perf;

use crate::{
    file::{
        PerfEvent, PerfEventFile, PerfGroup, SoftwareEvent, add_file_like, get_typed_file,
        perf::HardwareEvent,
    },
    mm::{UserMemoryCapability, map_usercopy_error},
    perf_security::{
        PerfAuthority, authorize_open, authorize_sampling_rate,
    },
    pmu_registry::{DynamicPmu, dynamic_pmu},
    task::{
        AsThread, PtraceAccessMode, check_current_thread_ptrace_image_access, get_visible_task,
    },
};
// Keep the probe bounded so a malicious size cannot make perf open perform
// unbounded usercopy work.  The v9 prefix itself is read field-by-field;
// fields absent from the caller's declared prefix are exactly zero.
const PERF_ATTR_EXTENSION_CHUNK_SIZE: usize = 64;

static NEXT_PERF_EVENT_ID: AtomicU64 = AtomicU64::new(1);

struct LoweredPerfEvent {
    event: PerfEvent,
    probe_name: Option<Arc<alloc::vec::Vec<u8>>>,
}

impl LoweredPerfEvent {
    const fn unnamed(event: PerfEvent) -> Self {
        Self {
            event,
            probe_name: None,
        }
    }
}

fn install_probe_query_name(file: &Arc<PerfEventFile>, lowered: &mut LoweredPerfEvent) {
    if let Some(name) = lowered.probe_name.take() {
        file.install_probe_query_name(name);
    }
}

/// `perf_event_attr::precise_ip` occupies flags[15:16].  The product PMU
/// publishes max_precise=1, so level 1 is the only PEBS guarantee we expose;
/// levels 2 and 3 must never collapse into that weaker contract.
#[inline]
const fn precise_ip_level(attr: &PerfEventAttr) -> u8 {
    ((attr.flags & linux_perf::ATTR_PRECISE_IP) >> 15) as u8
}

#[inline]
fn placement_policy(attr: &PerfEventAttr) -> crate::file::perf::PerfPlacementPolicy {
    crate::file::perf::PerfPlacementPolicy {
        pinned: attr.flags & linux_perf::ATTR_PINNED != 0,
        exclusive: attr.flags & linux_perf::ATTR_EXCLUSIVE != 0,
    }
}

/// Bind a raw encoding to the exact hybrid core type on which it was opened.
/// Later migration asks the PMU to place that same encoding; it must fail on
/// the other core type rather than reinterpret the raw config.
#[cfg(feature = "pmu")]
fn raw_core_type_from_snapshot(snapshot: axhal::pmu::CapabilitySnapshot) -> AxResult<u8> {
    if snapshot.product != axhal::pmu::ProductClass::PantherLake {
        return Err(AxError::OperationNotSupported);
    }
    match snapshot.core_type {
        axhal::pmu::IntelCoreType::Core => Ok(1),
        axhal::pmu::IntelCoreType::Atom => Ok(2),
        axhal::pmu::IntelCoreType::Unknown(_) => Err(AxError::OperationNotSupported),
    }
}

#[cfg(feature = "pmu")]
fn raw_core_type() -> AxResult<u8> {
    raw_core_type_from_snapshot(
        axhal::pmu::capability_snapshot().map_err(|_| AxError::OperationNotSupported)?,
    )
}

/// A raw event's encoding is bound to the CPU it will run on.  A task with
/// no CPU restriction keeps Linux's current-CPU open-time binding; all other
/// contexts must use the committed fleet record for their target CPU.
#[cfg(feature = "pmu")]
fn raw_core_type_for_target(target: PerfOpenTarget) -> AxResult<u8> {
    let cpu = match target.target {
        PerfTarget::Cpu { cpu } | PerfTarget::Cgroup { cpu, .. } => {
            usize::try_from(cpu).map_err(|_| AxError::InvalidInput)?
        }
        PerfTarget::Task { cpu, .. } if cpu >= 0 => {
            usize::try_from(cpu).map_err(|_| AxError::InvalidInput)?
        }
        PerfTarget::Task { .. } => return raw_core_type(),
    };
    raw_core_type_from_snapshot(
        axhal::pmu::fleet_capability_snapshot(cpu).map_err(|_| AxError::OperationNotSupported)?,
    )
}

/// Intel architectural events have SDM-defined event-select encodings and
/// CPUID.0AH EBX availability bits.  This table contains no model-specific
/// raw aliases; the PMU still checks the bit on the destination CPU.
fn architectural_hardware_event(event: linux_perf::Event) -> Option<HardwareEvent> {
    let (event_select, availability_bit) = match event {
        linux_perf::Event::HardwareCacheReferences => (0x4f2e, 3),
        linux_perf::Event::HardwareCacheMisses => (0x412e, 4),
        linux_perf::Event::HardwareBranchInstructions => (0x00c4, 5),
        linux_perf::Event::HardwareBranchMisses => (0x00c5, 6),
        linux_perf::Event::HardwareBusCycles => (0x013c, 7),
        linux_perf::Event::HardwareStalledFrontend => (0x01a3, 0),
        linux_perf::Event::HardwareStalledBackend => (0x02a3, 0),
        linux_perf::Event::HardwareRefCycles => (0x013c, 2),
        // PERF_TYPE_HW_CACHE packs cache | op<<8 | result<<16.  Only the
        // architecturally enumerated LLC read access/miss forms are generic;
        // all other cache triples require a typed PMU registry entry.
        linux_perf::Event::HardwareCache(2) => (0x4f2e, 3),
        linux_perf::Event::HardwareCache(0x1_0002) => (0x412e, 4),
        _ => return None,
    };
    Some(HardwareEvent::Architectural {
        event_select,
        availability_bit,
    })
}

#[cfg(not(feature = "pmu"))]
fn raw_core_type_for_target(_: PerfOpenTarget) -> AxResult<u8> {
    Err(AxError::OperationNotSupported)
}

fn raw_core_type_for_event(target: PerfOpenTarget, event_type: u32) -> AxResult<Option<u8>> {
    if event_type == linux_perf::PERF_TYPE_RAW
        || matches!(
            dynamic_pmu(event_type),
            Some(DynamicPmu::CpuCore | DynamicPmu::CpuAtom)
        )
    {
        return raw_core_type_for_target(target).map(Some);
    }
    Ok(None)
}

fn systemwide_event(event: linux_perf::Event, raw_core_type: Option<u8>) -> AxResult<PerfEvent> {
    Ok(match event {
        linux_perf::Event::SoftwareCpuClock => PerfEvent::Software(SoftwareEvent::CpuClock),
        linux_perf::Event::SoftwareTaskClock => PerfEvent::Software(SoftwareEvent::TaskClock),
        linux_perf::Event::SoftwarePageFaults => PerfEvent::Software(SoftwareEvent::PageFaults),
        linux_perf::Event::SoftwarePageFaultsMin => {
            PerfEvent::Software(SoftwareEvent::PageFaultsMin)
        }
        linux_perf::Event::SoftwarePageFaultsMaj => {
            PerfEvent::Software(SoftwareEvent::PageFaultsMaj)
        }
        linux_perf::Event::SoftwareCpuMigrations => {
            PerfEvent::Software(SoftwareEvent::CpuMigrations)
        }
        linux_perf::Event::SoftwareAlignmentFaults => {
            PerfEvent::Software(SoftwareEvent::AlignmentFaults)
        }
        linux_perf::Event::SoftwareEmulationFaults => {
            PerfEvent::Software(SoftwareEvent::EmulationFaults)
        }
        linux_perf::Event::SoftwareDummy => PerfEvent::Software(SoftwareEvent::Dummy),
        linux_perf::Event::SoftwareCgroupSwitches => {
            PerfEvent::Software(SoftwareEvent::CgroupSwitches)
        }
        linux_perf::Event::SoftwareContextSwitches => {
            PerfEvent::Software(SoftwareEvent::ContextSwitches)
        }
        linux_perf::Event::HardwareCycles | linux_perf::Event::HardwareInstructions => {
            #[cfg(not(feature = "pmu"))]
            return Err(AxError::OperationNotSupported);
            #[cfg(feature = "pmu")]
            {
                if axhal::pmu::capabilities().is_err() {
                    return Err(AxError::OperationNotSupported);
                }
                PerfEvent::Hardware(match event {
                    linux_perf::Event::HardwareCycles => HardwareEvent::Cycles,
                    linux_perf::Event::HardwareInstructions => HardwareEvent::Instructions,
                    _ => unreachable!(),
                })
            }
        }
        event if architectural_hardware_event(event).is_some() => {
            #[cfg(not(feature = "pmu"))]
            return Err(AxError::OperationNotSupported);
            #[cfg(feature = "pmu")]
            {
                axhal::pmu::capabilities().map_err(|_| AxError::OperationNotSupported)?;
                let hardware = architectural_hardware_event(event).unwrap();
                let HardwareEvent::Architectural {
                    availability_bit, ..
                } = hardware
                else {
                    unreachable!()
                };
                axhal::pmu::architectural_event_supported_fleet(availability_bit)
                    .map_err(|_| AxError::OperationNotSupported)?;
                PerfEvent::Hardware(hardware)
            }
        }
        linux_perf::Event::Tracepoint(id) => {
            crate::perf_sources::tracepoint(id)?;
            PerfEvent::Tracepoint(id)
        }
        linux_perf::Event::Raw(config) => PerfEvent::Raw {
            config,
            core_type: raw_core_type.ok_or(AxError::OperationNotSupported)?,
            precise: false,
            branch_stack: false,
        },
        linux_perf::Event::Breakpoint { addr, len, ty } => PerfEvent::Breakpoint { addr, len, ty },
        linux_perf::Event::Kprobe { function, offset } => {
            let addr = function.checked_add(offset).ok_or(AxError::InvalidInput)?;
            PerfEvent::Kprobe {
                addr,
                retprobe: false,
                query_offset: 0,
            }
        }
        // A uprobe pathname is a user pointer and therefore cannot be
        // resolved for a CPU/cgroup event (there is no task mm/FS context).
        linux_perf::Event::Uprobe { .. } => return Err(AxError::OperationNotSupported),
        linux_perf::Event::SoftwareBpfOutput => {
            return Err(AxError::OperationNotSupported);
        }
        // A guarded architectural-PMU arm cannot make the match exhaustive.
        // Reject every unrecognised encoding instead of fabricating a counter.
        _ => return Err(AxError::OperationNotSupported),
    })
}

/// Lowers a source whose target is a CPU or cgroup. The pathname operand is
/// resolved exactly once in the opening task's FS/user-memory context; only
/// the resulting stable file identity enters the system-wide probe registry.
fn systemwide_event_with_uprobe(
    event: linux_perf::Event,
    raw_core_type: Option<u8>,
    memory: &UserMemoryCapability,
) -> AxResult<LoweredPerfEvent> {
    match event {
        linux_perf::Event::Uprobe {
            path,
            offset,
            retprobe,
        } => {
            let file = crate::perf_sources::resolve_uprobe_inode(
                memory,
                path as *const u8,
                current().id().as_u64(),
            )?;
            Ok(LoweredPerfEvent {
                event: PerfEvent::Uprobe {
                    mount_id: file.key.mount_id,
                    device: file.key.device,
                    inode: file.key.inode,
                    offset,
                    retprobe,
                    reference_counter_offset: 0,
                },
                probe_name: Some(file.name),
            })
        }
        event => systemwide_event(event, raw_core_type).map(LoweredPerfEvent::unnamed),
    }
}

/// Lower a sysfs-published type through its exact registry descriptor.  These
/// types are runtime numbers, not Linux's fixed PERF_TYPE_* namespace.
#[cfg(feature = "pmu")]
fn dynamic_pmu_event(
    source: DynamicPmu,
    config: u64,
    precise: bool,
    branch_stack: bool,
) -> AxResult<PerfEvent> {
    match source {
        DynamicPmu::CpuCore | DynamicPmu::CpuAtom => {
            let expected = if matches!(source, DynamicPmu::CpuCore) {
                1
            } else {
                2
            };
            Ok(PerfEvent::Raw {
                config,
                core_type: expected,
                precise,
                branch_stack,
            })
        }
        DynamicPmu::Uncore { box_type, box_id } => Ok(PerfEvent::Uncore {
            box_type,
            box_id,
            config,
        }),
        DynamicPmu::ReadOnly(pmu) => Ok(PerfEvent::ReadOnly { pmu, config }),
        // AUX transport is admitted separately before descriptor creation;
        // it is not an ordinary scalar counter and therefore requires the
        // sampling/AUX lane rather than this counting constructor.
        DynamicPmu::IntelPt | DynamicPmu::IntelBts => Err(AxError::OperationNotSupported),
        // Probe PMUs use config1/config2, not the raw config field. They are
        // lowered by `dynamic_probe_event` after full attr parsing.
        DynamicPmu::Kprobe | DynamicPmu::Uprobe => Err(AxError::InvalidInput),
    }
}

fn dynamic_probe_event(
    source: DynamicPmu,
    attr: &PerfEventAttr,
    memory: &UserMemoryCapability,
    task_id: u64,
) -> AxResult<Option<LoweredPerfEvent>> {
    match source {
        DynamicPmu::Kprobe => {
            if attr.config1 == 0 {
                return Err(AxError::InvalidInput);
            }
            let (addr, probe_name) = if crate::syscall::task::is_direct_kprobe_address(attr.config1)
            {
                // For direct-address kprobes config1 is the exact address;
                // config2 is meaningful only for symbol-based probes.
                let address = attr.config1;
                crate::syscall::task::validate_kprobe_address(address)?;
                (address, None)
            } else {
                let name = memory
                    .load_until_nul_bounded(
                        attr.config1 as *const u8,
                        crate::syscall::task::KPROBE_SYMBOL_MAX,
                    )
                    .map_err(map_usercopy_error)?;
                let symbol = core::str::from_utf8(&name).map_err(|_| AxError::InvalidInput)?;
                let address = crate::syscall::task::resolve_kprobe_symbol(symbol, attr.config2)?;
                (
                    address,
                    Some(Arc::try_new(name).map_err(|_| AxError::NoMemory)?),
                )
            };
            Ok(Some(LoweredPerfEvent {
                event: PerfEvent::Kprobe {
                    addr,
                    retprobe: attr.config & 1 != 0,
                    query_offset: if probe_name.is_some() {
                        attr.config2
                    } else {
                        0
                    },
                },
                probe_name,
            }))
        }
        DynamicPmu::Uprobe => {
            if attr.config1 == 0 {
                return Err(AxError::InvalidInput);
            }
            let file = crate::perf_sources::resolve_uprobe_inode(
                memory,
                attr.config1 as *const u8,
                task_id,
            )?;
            Ok(Some(LoweredPerfEvent {
                event: PerfEvent::Uprobe {
                    mount_id: file.key.mount_id,
                    device: file.key.device,
                    inode: file.key.inode,
                    offset: attr.config2,
                    retprobe: attr.config & 1 != 0,
                    reference_counter_offset: attr.config >> 32,
                },
                probe_name: Some(file.name),
            }))
        }
        _ => Ok(None),
    }
}

#[derive(Clone, Copy)]
struct PmuPlannerSnapshot {
    available: bool,
    max_sample_period: u64,
}

#[cfg(feature = "pmu")]
fn pmu_planner_snapshot() -> PmuPlannerSnapshot {
    match axhal::pmu::capabilities() {
        Ok(capabilities) => PmuPlannerSnapshot {
            available: true,
            max_sample_period: capabilities.programmable_mask(),
        },
        Err(_) => PmuPlannerSnapshot {
            available: false,
            max_sample_period: 0,
        },
    }
}

#[cfg(not(feature = "pmu"))]
const fn pmu_planner_snapshot() -> PmuPlannerSnapshot {
    PmuPlannerSnapshot {
        available: false,
        max_sample_period: 0,
    }
}

fn map_perf_reject(reject: linux_perf::Reject) -> AxError {
    match reject {
        linux_perf::Reject::SizeTooSmall
        | linux_perf::Reject::SizeTooLarge
        | linux_perf::Reject::NonZeroTail => AxError::ArgumentListTooLong,
        linux_perf::Reject::UnknownOpenFlags
        | linux_perf::Reject::InvalidExclusion
        | linux_perf::Reject::InvalidPeriod
        | linux_perf::Reject::InvalidWakeup
        | linux_perf::Reject::InvalidBreakpoint
        | linux_perf::Reject::InvalidEvent
        | linux_perf::Reject::InvalidSamplingMode => AxError::InvalidInput,
        linux_perf::Reject::UnknownAttrFlags
        | linux_perf::Reject::UnsupportedAttrFlags
        | linux_perf::Reject::UnsupportedOpenFlags
        | linux_perf::Reject::UnsupportedReadFormat
        | linux_perf::Reject::UnknownReadFormat
        | linux_perf::Reject::UnknownSampleType
        | linux_perf::Reject::UnsupportedSampleType
        | linux_perf::Reject::UnsupportedEvent
        | linux_perf::Reject::UnsupportedSampling
        | linux_perf::Reject::UnsupportedGroup => AxError::OperationNotSupported,
    }
}

fn map_perf_attr_reject(reject: linux_perf::AttrReject) -> AxError {
    match reject {
        linux_perf::AttrReject::SizeTooSmall
        | linux_perf::AttrReject::SizeTooLarge
        | linux_perf::AttrReject::NonZeroTail
        | linux_perf::AttrReject::UnsupportedAttrVersion => AxError::ArgumentListTooLong,
        linux_perf::AttrReject::UnknownOpenFlags
        | linux_perf::AttrReject::UnknownAttrFlags
        | linux_perf::AttrReject::UnknownReadFormat
        | linux_perf::AttrReject::UnknownSampleType
        | linux_perf::AttrReject::InvalidTarget
        | linux_perf::AttrReject::InvalidPeriod
        | linux_perf::AttrReject::InvalidWakeup
        | linux_perf::AttrReject::InvalidFlags
        | linux_perf::AttrReject::InvalidExtension => AxError::InvalidInput,
        linux_perf::AttrReject::UnsupportedOpenFlags
        | linux_perf::AttrReject::UnsupportedAttrFlags
        | linux_perf::AttrReject::UnsupportedReadFormat
        | linux_perf::AttrReject::UnsupportedSampleType
        | linux_perf::AttrReject::UnsupportedEvent
        | linux_perf::AttrReject::UnsupportedTarget => AxError::OperationNotSupported,
    }
}

fn perf_capabilities(exact_aux: bool) -> PerfCapabilities {
    let pmu = pmu_planner_snapshot();
    // Trace/probe/software sources publish through the data-ring without a
    // core PMU, so TCG and PMU-less machines still expose sampling semantics.
    let sampling = cfg!(feature = "perf-sampling");
    PerfCapabilities {
        // The syscall always parses the complete published v9 prefix.  Each
        // semantic bit remains separately gated below; accepting a later
        // prefix is not an accidental promise to implement it.
        max_attr_size: linux_perf::PERF_ATTR_SIZE_VER9,
        event_types: (pmu.available as u64
            * linux_perf::perf_type_bit(linux_perf::PERF_TYPE_HARDWARE))
            | linux_perf::perf_type_bit(linux_perf::PERF_TYPE_SOFTWARE)
            | linux_perf::perf_type_bit(linux_perf::PERF_TYPE_TRACEPOINT)
            | linux_perf::perf_type_bit(linux_perf::PERF_TYPE_HW_CACHE)
            | linux_perf::perf_type_bit(linux_perf::PERF_TYPE_RAW)
            | linux_perf::perf_type_bit(linux_perf::PERF_TYPE_BREAKPOINT),
        attr_flags: linux_perf::ATTR_DISABLED
            | linux_perf::ATTR_PINNED
            | linux_perf::ATTR_EXCLUSIVE
            | linux_perf::ATTR_EXCLUDE_USER
            | linux_perf::ATTR_EXCLUDE_KERNEL
            | linux_perf::ATTR_MMAP
            | linux_perf::ATTR_COMM
            | linux_perf::ATTR_MMAP_DATA
            | linux_perf::ATTR_MMAP2
            | linux_perf::ATTR_COMM_EXEC
            | linux_perf::ATTR_CONTEXT_SWITCH
            | linux_perf::ATTR_INHERIT
            | linux_perf::ATTR_INHERIT_THREAD
            | linux_perf::ATTR_ENABLE_ON_EXEC
            | linux_perf::ATTR_REMOVE_ON_EXEC
            | linux_perf::ATTR_TASK
            | linux_perf::ATTR_SAMPLE_ID_ALL
            | linux_perf::ATTR_FREQ
            | linux_perf::ATTR_USE_CLOCKID
            | linux_perf::ATTR_WATERMARK
            | if exact_aux {
                linux_perf::ATTR_PRECISE_IP
            } else {
                0
            },
        sample_type: if sampling {
            linux_perf::PERF_SAMPLE_IP
                | linux_perf::PERF_SAMPLE_IDENTIFIER
                | linux_perf::PERF_SAMPLE_TID
                | linux_perf::PERF_SAMPLE_TIME
                | linux_perf::PERF_SAMPLE_ID
                | linux_perf::PERF_SAMPLE_CPU
                | linux_perf::PERF_SAMPLE_PERIOD
                | linux_perf::PERF_SAMPLE_STREAM_ID
                | linux_perf::PERF_SAMPLE_RAW
                | if exact_aux {
                    linux_perf::PERF_SAMPLE_AUX
                        | linux_perf::PERF_SAMPLE_BRANCH_STACK
                        | linux_perf::PERF_SAMPLE_ADDR
                        | linux_perf::PERF_SAMPLE_DATA_SRC
                } else {
                    0
                }
        } else {
            0
        },
        read_format: linux_perf::PERF_FORMAT_IMPLEMENTED,
        open_flags: linux_perf::PERF_FLAG_FD_CLOEXEC
            | linux_perf::PERF_FLAG_FD_NO_GROUP
            | linux_perf::PERF_FLAG_FD_OUTPUT
            | linux_perf::PERF_FLAG_PID_CGROUP,
        branch_sample_type: if exact_aux { u64::MAX } else { 0 },
        regs_user_mask: 0,
        regs_intr_mask: 0,
        supports_frequency: true,
        supports_watermark: true,
        supports_group: true,
        supports_output: cfg!(feature = "perf-sampling"),
        supports_cgroup: true,
        supports_aux: exact_aux,
        supports_sigtrap: false,
    }
}

/// Lower source types whose Linux operands live beyond the V0 prefix.  This
/// is deliberately separate from the historical V0 record-layout planner:
/// copying `config1/config2` into a V0 value would lose breakpoint/probe
/// semantics and falsely turn them into another event.
fn dynamic_source_plan(
    attr: PerfEventAttr,
    close_on_exec: bool,
) -> AxResult<Option<linux_perf::PerfOpenPlan>> {
    let event = match attr.event_type {
        linux_perf::PERF_TYPE_RAW => linux_perf::Event::Raw(attr.config),
        linux_perf::PERF_TYPE_BREAKPOINT => {
            if attr.config1 == 0
                || !matches!(attr.config2, 1 | 2 | 4 | 8)
                || attr.bp_type == 0
                || attr.bp_type & !7 != 0
            {
                return Err(AxError::InvalidInput);
            }
            linux_perf::Event::Breakpoint {
                addr: attr.config1,
                len: attr.config2,
                ty: attr.bp_type,
            }
        }
        _ => return Ok(None),
    };
    let sample = match (attr.sample_period, attr.sample_type) {
        (0, 0) => None,
        (0, _) | (_, 0) => return Err(AxError::InvalidInput),
        (period, sample_type) => Some(linux_perf::SampleRecordPlan {
            period,
            sample_type,
            // The data-ring encoder derives exact layout from sample_type;
            // these legacy summary fields are not consumed by the backend.
            fixed_words: 0,
            has_raw: false,
            has_callchain: false,
            read: None,
        }),
    };
    Ok(Some(linux_perf::PerfOpenPlan {
        event,
        disabled: attr.flags & linux_perf::ATTR_DISABLED != 0,
        exclude_user: attr.flags & linux_perf::ATTR_EXCLUDE_USER != 0,
        exclude_kernel: attr.flags & linux_perf::ATTR_EXCLUDE_KERNEL != 0,
        close_on_exec,
        lifecycle: linux_perf::PerfLifecycle::from_flags(attr.flags),
        sample,
        read: linux_perf::ReadPlan {
            group: attr.read_format & linux_perf::PERF_FORMAT_GROUP != 0,
            time_enabled: attr.read_format & linux_perf::PERF_FORMAT_TOTAL_TIME_ENABLED != 0,
            time_running: attr.read_format & linux_perf::PERF_FORMAT_TOTAL_TIME_RUNNING != 0,
            id: attr.read_format & linux_perf::PERF_FORMAT_ID != 0,
            lost: attr.read_format & linux_perf::PERF_FORMAT_LOST != 0,
        },
    }))
}

pub(crate) fn perf_plan(
    attr: PerfEventAttr,
    size: u32,
    extra_tail: &[u8],
    target: PerfOpenTarget,
) -> AxResult<(linux_perf::PerfAttrPlan, linux_perf::PerfOpenPlan)> {
    let pmu = pmu_planner_snapshot();
    let exact_aux = exact_aux_candidate();
    let registry_dynamic = dynamic_pmu(attr.event_type);
    // The ABI schema uses a compact fixed-type bitset, whereas perf sysfs
    // assigns dynamic type numbers (including numbers above 63).  Validate a
    // registry type with RAW's config-only schema, after first proving that
    // the type is currently published by the committed registry.
    let mut schema_attr = attr;
    if registry_dynamic.is_some() {
        if matches!(
            registry_dynamic,
            Some(DynamicPmu::IntelPt | DynamicPmu::IntelBts)
        ) {
            // PT/BTS are transport PMUs, not scalar raw counters.  Their
            // dynamic type selects the already-discovered AUX backend.  The
            // generic sampling planner still needs an ordinary overflow
            // clock, so it sees a cycles surrogate; the untouched `attr` is
            // passed to AuxRequest afterwards, where Intel PT's native
            // config and config1/config2 admission is performed.  In
            // particular, never reject PT config here before its PMU owns
            // the interpretation.
            schema_attr.event_type = linux_perf::PERF_TYPE_HARDWARE;
            schema_attr.config = linux_perf::PERF_COUNT_HW_CPU_CYCLES;
        } else {
            schema_attr.event_type = linux_perf::PERF_TYPE_RAW;
        }
    }

    let capabilities = perf_capabilities(exact_aux);
    let attr_flags = capabilities.attr_flags;

    #[cfg(feature = "pmu")]
    if let Some(DynamicPmu::ReadOnly(pmu)) = registry_dynamic {
        let source = axhal::perf_uncore::readonly_pmus()
            .find(|source| source.pmu == pmu)
            .ok_or(AxError::OperationNotSupported)?;
        // Package RAPL and package C-state MSRs are owned by one CPU.  The
        // counting/read paths have no read-only remote reconcile lease, so
        // reject at open rather than creating an FD that fails later.
        if source.package_scoped && !readonly_target_is_owned(target.target, source.owner_cpu) {
            return Err(AxError::OperationNotSupported);
        }
    }

    let attr_plan = linux_perf::plan_attr(
        PerfAttrInput {
            attr: schema_attr,
            supplied_size: size,
            extra_tail,
            target,
        },
        perf_capabilities(exact_aux),
    )
    .map_err(map_perf_attr_reject)?;
    if precise_ip_level(&attr) > 1 {
        // ATTR_PRECISE_IP is a 2-bit field.  `exact_aux` only means that the
        // hardware has a level-1 PEBS path; it is not permission to round a
        // level-2/3 request down to the weaker guarantee.
        return Err(AxError::OperationNotSupported);
    }
    // The complete planner has already interpreted every ABI extension. The
    // current event backend has no config2/config3/config4 transport, so do
    // not silently drop any of them while lowering to its record plan.
    let dynamic_source = registry_dynamic.is_some()
        || matches!(
            attr.event_type,
            linux_perf::PERF_TYPE_RAW | linux_perf::PERF_TYPE_BREAKPOINT
        );
    if (!dynamic_source && (attr_plan.extensions.config1 != 0 || attr_plan.extensions.config2 != 0))
        || attr_plan.extensions.config3 != 0
        || attr_plan.extensions.config4 != 0
        || attr_plan.extensions.sample_regs_user != 0
        || attr_plan.extensions.sample_stack_user != 0
        || (attr.flags & linux_perf::ATTR_USE_CLOCKID != 0
            && attr_plan.extensions.clockid != Some(linux_raw_sys::general::CLOCK_MONOTONIC as i32))
        || attr_plan.extensions.sample_regs_intr != 0
        || attr_plan.extensions.sample_max_stack != 0
        || attr_plan.extensions.sig_data != 0
    {
        return Err(AxError::OperationNotSupported);
    }

    // The timestamp backend already implements the admitted monotonic clock.
    // The V0 record planner has no clock field; do not ask it to interpret
    // the extension a second time after the complete planner validated it.
    schema_attr.flags &= !linux_perf::ATTR_USE_CLOCKID;
    let v0 = PerfEventAttrV0::from(schema_attr);
    let legacy = if let Some(plan) = dynamic_source_plan(
        schema_attr,
        target.open_flags & linux_perf::PERF_FLAG_FD_CLOEXEC != 0,
    )? {
        plan
    } else {
        linux_perf::plan(
            linux_perf::PerfInput {
                attr: v0,
                supplied_size: PERF_ATTR_SIZE_VER0,
                tail_nonzero: false,
                // FD_NO_GROUP changes only descriptor-table grouping.  The older
                // event-layout planner predates that target distinction, so lower
                // it after the full planner has validated it.
                open_flags: target.open_flags & !PERF_FLAG_FD_NO_GROUP,
            },
            linux_perf::PerfSnapshot {
                max_sample_period: pmu.max_sample_period,
            },
            linux_perf::FeatureSet {
                hardware: pmu.available,
                software: true,
                raw: true,
                tracepoint: true,
                breakpoint: true,
                kprobe: true,
                uprobe: true,
                sampling: cfg!(feature = "perf-sampling"),
                attr_flags,
                read_format: linux_perf::PERF_FORMAT_IMPLEMENTED,
                open_flags: capabilities.open_flags,
                sample_type: capabilities.sample_type,
                // Software/tracepoint periods count source occurrences and
                // must allow one record per edge. Only hardware overflows
                // need the counter interrupt-rate floor.
                min_sample_period: if matches!(
                    schema_attr.event_type,
                    linux_perf::PERF_TYPE_SOFTWARE | linux_perf::PERF_TYPE_TRACEPOINT
                ) { 1 } else { 4096 },
                max_wakeup_events: u32::MAX,
                sampling_read_format: linux_perf::PERF_FORMAT_IMPLEMENTED,
                sampling_requires_zero_config1: true,
                // Source-backed events use the same preallocated mmap ring;
                // they do not arm a counter, so do not reject their sampling
                // attributes in the legacy lowering pass.
                sampling_hardware_only: false,
            },
        )
        .map_err(map_perf_reject)?
    };
    Ok((attr_plan, legacy))
}

#[cfg(feature = "pmu")]
const fn readonly_target_is_owned(target: PerfTarget, owner_cpu: usize) -> bool {
    matches!(target, PerfTarget::Cpu { cpu } if cpu >= 0 && cpu as usize == owner_cpu)
}

/// This is only a candidate gate for schema validation.  `AuxRequest::admit`
/// performs the final CPUID/backend-specific admission before an FD exists.
#[cfg(all(feature = "pmu", target_os = "none"))]
fn exact_aux_candidate() -> bool {
    matches!(
        axhal::pmu::capability_snapshot(),
        Ok(snapshot) if snapshot.product == axhal::pmu::ProductClass::PantherLake
    )
}

#[cfg(not(all(feature = "pmu", target_os = "none")))]
const fn exact_aux_candidate() -> bool {
    false
}

#[cfg(feature = "perf-sampling")]
fn open_sampling(
    plan: linux_perf::PerfOpenPlan,
    attr: PerfEventAttr,
    target: PerfOpenTarget,
    authority: PerfAuthority,
    memory: &UserMemoryCapability,
    frequency: Option<u64>,
    output_fd: Option<i32>,
    aux: Option<crate::file::perf_aux::AuxRequest>,
) -> AxResult<isize> {
    let output_target = output_fd
        .map(crate::file::get_typed_file::<PerfEventFile>)
        .transpose()?
        // Keep the typed object while retaining the FD lookup's identity.
        // `FileHandle::file` is deliberately private to the file subsystem;
        // callers must not reach through the descriptor representation.
        .map(|handle| handle.clone_object());
    let sample = plan.sample.ok_or(AxError::OperationNotSupported)?;
    let id = NEXT_PERF_EVENT_ID.fetch_add(1, Ordering::Relaxed);
    let mut attached_task = None;
    let mut target_task = None;
    let (group, target_task_id, attached_cpu_context) = match target.target {
        PerfTarget::Cgroup { fd, cpu } => {
            let cpu = usize::try_from(cpu).map_err(|_| AxError::InvalidInput)?;
            let context = crate::file::perf::PerfContext::Cgroup {
                cgroup_id: crate::pseudofs::cgroup::perf_cgroup_fd_identity(fd)?,
                cpu,
            };
            let (group, attached_cpu_context) = if target.group_fd >= 0 {
                let leader = get_typed_file::<PerfEventFile>(target.group_fd)?;
                let group = leader.group().ok_or(AxError::BadFileDescriptor)?;
                if !leader.is_group_leader() || group.context() != context {
                    return Err(AxError::InvalidInput);
                }
                (group, false)
            } else {
                let group = PerfGroup::new_for_context(context, id)?;
                (group, true)
            };
            (group, 0, attached_cpu_context)
        }
        PerfTarget::Cpu { cpu } => {
            let cpu = usize::try_from(cpu).map_err(|_| AxError::InvalidInput)?;
            let context = crate::file::perf::PerfContext::Cpu { cpu };
            let (group, attached_cpu_context) = if target.group_fd >= 0 {
                let leader = get_typed_file::<PerfEventFile>(target.group_fd)?;
                let group = leader.group().ok_or(AxError::BadFileDescriptor)?;
                if !leader.is_group_leader() || group.context() != context {
                    return Err(AxError::InvalidInput);
                }
                (group, false)
            } else {
                let group = PerfGroup::new_for_context(context, id)?;
                (group, true)
            };
            (group, 0, attached_cpu_context)
        }
        PerfTarget::Task { pid, cpu } => {
            let task = if pid == 0 {
                current().clone()
            } else {
                let tid = current()
                    .as_thread()
                    .pid_ns()
                    .resolve_visible_pid(pid as u32)
                    .ok_or(AxError::NoSuchProcess)?;
                get_visible_task(tid)?
            };
            target_task = Some(task.clone());
            check_current_thread_ptrace_image_access(task.as_thread(), PtraceAccessMode::ReadReal)?;
            let task_id = task.id().as_u64();
            let context = if cpu == -1 {
                crate::file::perf::PerfContext::Task { task_id }
            } else {
                crate::file::perf::PerfContext::TaskOnCpu {
                    task_id,
                    cpu: usize::try_from(cpu).map_err(|_| AxError::InvalidInput)?,
                }
            };
            let (group, attached_cpu_context) = if target.group_fd >= 0 {
                let leader = get_typed_file::<PerfEventFile>(target.group_fd)?;
                let group = leader.group().ok_or(AxError::BadFileDescriptor)?;
                if !leader.is_group_leader() || group.context() != context {
                    return Err(AxError::InvalidInput);
                }
                (group, false)
            } else {
                let group = PerfGroup::new_for_context(context, id)?;
                attached_task = Some(task.clone());
                (group, false)
            };
            (group, task_id, attached_cpu_context)
        }
    };
    // Dynamic PMU types have operands that the generic record planner cannot
    // represent.  Lower them only after target selection: raw encodings bind
    // to the target CPU, and uprobes resolve/install in the target mm.
    let built_event = (|| -> AxResult<_> {
        if let Some(source) = dynamic_pmu(attr.event_type) {
            match source {
                DynamicPmu::CpuCore | DynamicPmu::CpuAtom => {
                    #[cfg(not(feature = "pmu"))]
                    return Err(AxError::OperationNotSupported);
                    #[cfg(feature = "pmu")]
                    {
                        let expected = if matches!(source, DynamicPmu::CpuCore) {
                            1
                        } else {
                            2
                        };
                        let core_type = match expected {
                            1 => axhal::pmu::IntelCoreType::Core,
                            2 => axhal::pmu::IntelCoreType::Atom,
                            _ => unreachable!(),
                        };
                        return Ok((
                            crate::file::perf_sampling::SamplingEvent::Raw {
                                config: attr.config,
                                core_type,
                            },
                            LoweredPerfEvent::unnamed(dynamic_pmu_event(
                                source,
                                attr.config,
                                precise_ip_level(&attr) == 1,
                                attr.branch_sample_type != 0,
                            )?),
                        ));
                    }
                }
                DynamicPmu::Kprobe | DynamicPmu::Uprobe => {
                    let event = dynamic_probe_event(source, &attr, memory, target_task_id)?
                        .ok_or(AxError::InvalidInput)?;
                    return Ok((crate::file::perf_sampling::SamplingEvent::Source, event));
                }
                DynamicPmu::IntelPt | DynamicPmu::IntelBts => {
                    // AUX owns transport/config admission; cycles provides the
                    // ordinary overflow clock for its data-ring metadata.
                    if aux.is_none() {
                        return Err(AxError::OperationNotSupported);
                    }
                    return Ok((
                        crate::file::perf_sampling::SamplingEvent::Cycles,
                        LoweredPerfEvent::unnamed(PerfEvent::Hardware(HardwareEvent::Cycles)),
                    ));
                }
                DynamicPmu::Uncore { .. } | DynamicPmu::ReadOnly(_) => {
                    return Err(AxError::OperationNotSupported);
                }
            }
        }
        let lowered = match plan.event {
            linux_perf::Event::HardwareCycles => {
                LoweredPerfEvent::unnamed(PerfEvent::Hardware(HardwareEvent::Cycles))
            }
            linux_perf::Event::HardwareInstructions => {
                LoweredPerfEvent::unnamed(PerfEvent::Hardware(HardwareEvent::Instructions))
            }
            linux_perf::Event::Raw(config) => {
                let core_type = raw_core_type_for_target(target)?;
                let sampling_core_type = match core_type {
                    1 => axhal::pmu::IntelCoreType::Core,
                    2 => axhal::pmu::IntelCoreType::Atom,
                    value => axhal::pmu::IntelCoreType::Unknown(value),
                };
                return Ok((
                    crate::file::perf_sampling::SamplingEvent::Raw {
                        config,
                        core_type: sampling_core_type,
                    },
                    LoweredPerfEvent::unnamed(PerfEvent::Raw {
                        config,
                        core_type,
                        precise: precise_ip_level(&attr) == 1,
                        branch_stack: attr.branch_sample_type != 0,
                    }),
                ));
            }
            other => systemwide_event_with_uprobe(other, None, memory)?,
        };
        let sampling_event = match plan.event {
            linux_perf::Event::HardwareCycles => crate::file::perf_sampling::SamplingEvent::Cycles,
            linux_perf::Event::HardwareInstructions => {
                crate::file::perf_sampling::SamplingEvent::Instructions
            }
            linux_perf::Event::Tracepoint(_)
            | linux_perf::Event::Breakpoint { .. }
            | linux_perf::Event::Kprobe { .. }
            | linux_perf::Event::Uprobe { .. }
            | linux_perf::Event::SoftwareCpuClock
            | linux_perf::Event::SoftwareTaskClock
            | linux_perf::Event::SoftwarePageFaults
            | linux_perf::Event::SoftwarePageFaultsMin
            | linux_perf::Event::SoftwarePageFaultsMaj
            | linux_perf::Event::SoftwareContextSwitches
            | linux_perf::Event::SoftwareCpuMigrations
            | linux_perf::Event::SoftwareAlignmentFaults
            | linux_perf::Event::SoftwareEmulationFaults
            | linux_perf::Event::SoftwareDummy
            | linux_perf::Event::SoftwareCgroupSwitches => {
                crate::file::perf_sampling::SamplingEvent::Source
            }
            _ => return Err(AxError::OperationNotSupported),
        };
        Ok((sampling_event, lowered))
    })();
    let (sampling_event, mut lowered) = match built_event {
        Ok(event) => event,
        Err(error) => {
            if let Some(task) = attached_task.as_ref() {
                task.as_thread().detach_empty_perf_group(&group);
            } else if attached_cpu_context {
                PerfGroup::detach_empty_cpu_context(&group);
            }
            return Err(error);
        }
    };
    let event = lowered.event;
    if !sampling_fields_supported_by_backend(&attr, sampling_event) {
        if let Some(task) = attached_task.as_ref() {
            task.as_thread().detach_empty_perf_group(&group);
        } else if attached_cpu_context {
            PerfGroup::detach_empty_cpu_context(&group);
        }
        return Err(AxError::OperationNotSupported);
    }
    // ATTR_FREQ's union member is Hz, never a counter preload.  Start from a
    // bounded counter period; each completion adjusts it from observed wall
    // time toward the requested rate.
    let initial_period = if frequency.is_some() {
        100_000
    } else {
        sample.period
    };
    let backend =
        match crate::file::PerfSampleBackend::try_new(crate::file::perf_sampling::SamplingConfig {
            id,
            target_task_id,
            event: sampling_event,
            period: initial_period,
            frequency,
            sample_type: sample.sample_type,
            count_user: !plan.exclude_user,
            count_kernel: !plan.exclude_kernel,
            disabled: plan.disabled,
            read_format: plan.read.bits(),
            aux,
            identity: crate::file::perf_sampling::PerfOpenIdentity {
                attr,
                target,
                authority,
            },
        }) {
            Ok(backend) => backend,
            Err(error) => {
                if let Some(task) = attached_task.as_ref() {
                    task.as_thread().detach_empty_perf_group(&group);
                } else if attached_cpu_context {
                    PerfGroup::detach_empty_cpu_context(&group);
                }
                return Err(error);
            }
        };
    let file = match PerfEventFile::new_sampling_placement(
        id,
        event,
        &group,
        plan.read,
        plan.lifecycle,
        placement_policy(&attr),
        backend,
    ) {
        Ok(file) => file,
        Err(error) => {
            if let Some(task) = attached_task.as_ref() {
                task.as_thread().detach_empty_perf_group(&group);
            } else if attached_cpu_context {
                PerfGroup::detach_empty_cpu_context(&group);
            }
            return Err(error);
        }
    };
    install_probe_query_name(&file, &mut lowered);
    if let PerfEvent::Uprobe {
        mount_id,
        device,
        inode,
        offset,
        retprobe,
        reference_counter_offset,
    } = event
    {
        if let Some(task) = target_task.as_ref() {
            if let Err(error) = crate::uprobe::install_for_mm(
                &task.as_thread().proc_data.aspace(),
                crate::uprobe::UprobeFileKey {
                    mount_id,
                    device,
                    inode,
                },
                offset,
                retprobe,
                reference_counter_offset,
            ) {
                drop(file);
                if let Some(attached_task) = attached_task.as_ref() {
                    attached_task.as_thread().detach_empty_perf_group(&group);
                }
                return Err(error);
            }
        }
    }
    if let Some(output_target) = output_target {
        if let Err(error) = file.set_output_target(output_target) {
            drop(file);
            if let Some(task) = attached_task.as_ref() {
                task.as_thread().detach_empty_perf_group(&group);
            } else if attached_cpu_context {
                PerfGroup::detach_empty_cpu_context(&group);
            }
            return Err(error);
        }
    }
    // Publish only after the first member exists. Scheduler registry pruning
    // must never observe a newly opened group as empty during construction.
    if let Some(task) = attached_task.as_ref() {
        task.as_thread().attach_perf_group(group.clone())?;
    } else if attached_cpu_context {
        PerfGroup::attach_cpu_context(&group)?;
    }
    match add_file_like(file as Arc<dyn crate::file::FileLike>, plan.close_on_exec) {
        Ok(fd) => {
            // Existing current-task groups have already crossed their enter
            // edge; CPU/cgroup placement is driven by its scheduler context.
            if attached_task
                .as_ref()
                .is_some_and(|task| task.id() == current().id())
            {
                group.on_enter();
            } else if target_task
                .as_ref()
                .is_some_and(|task| task.id() == current().id())
            {
                group.reconfigure_current();
            }
            Ok(fd as isize)
        }
        Err(error) => {
            if let Some(task) = attached_task.as_ref() {
                task.as_thread().detach_empty_perf_group(&group);
            } else if attached_cpu_context {
                PerfGroup::detach_empty_cpu_context(&group);
            }
            Err(error)
        }
    }
}

/// Refine the broad PMU capability mask by the selected producer.  The mask
/// is intentionally broad enough for PEBS/LBR hardware samples, but source
/// records must not inherit fields that they cannot capture.
#[cfg(feature = "perf-sampling")]
pub(crate) fn sampling_fields_supported_by_backend(
    attr: &PerfEventAttr,
    event: crate::file::perf_sampling::SamplingEvent,
) -> bool {
    let exact_fields = linux_perf::PERF_SAMPLE_ADDR | linux_perf::PERF_SAMPLE_DATA_SRC;
    let lbr_fields = linux_perf::PERF_SAMPLE_BRANCH_STACK;
    if matches!(event, crate::file::perf_sampling::SamplingEvent::Source)
        && (attr.sample_type & (exact_fields | lbr_fields) != 0 || precise_ip_level(attr) != 0)
    {
        return false;
    }
    // PEBS is the sole producer for these two words.  They are not ordinary
    // counter metadata, so a request without exact-IP must fail instead of
    // receiving a fabricated zero address/data-source pair.
    attr.sample_type & exact_fields == 0 || precise_ip_level(attr) == 1
}

pub(crate) fn read_attr(
    memory: &UserMemoryCapability,
    attr: *const PerfEventAttr,
    size: usize,
) -> AxResult<PerfEventAttr> {
    if attr.is_null() {
        return Err(AxError::BadAddress);
    }
    let v0 = memory
        .read_value_uninit(attr.cast::<PerfEventAttrV0>())
        .map_err(map_usercopy_error)
        .map(|value| unsafe { value.assume_init() })?;
    let mut out = PerfEventAttr::from(v0);
    macro_rules! extension {
        ($field:ident, $offset:expr, $ty:ty) => {
            if size >= $offset + core::mem::size_of::<$ty>() {
                let address = (attr as usize)
                    .checked_add($offset)
                    .ok_or(AxError::BadAddress)?;
                out.$field = memory
                    .read_value_uninit(address as *const $ty)
                    .map_err(map_usercopy_error)
                    .map(|value| unsafe { value.assume_init() })?;
            }
        };
    }
    extension!(config2, linux_perf::PERF_ATTR_CONFIG2_OFFSET, u64);
    extension!(
        branch_sample_type,
        linux_perf::PERF_ATTR_BRANCH_SAMPLE_TYPE_OFFSET,
        u64
    );
    extension!(
        sample_regs_user,
        linux_perf::PERF_ATTR_SAMPLE_REGS_USER_OFFSET,
        u64
    );
    extension!(sample_stack_user, 88, u32);
    extension!(clockid, 92, i32);
    extension!(
        sample_regs_intr,
        linux_perf::PERF_ATTR_SAMPLE_REGS_INTR_OFFSET,
        u64
    );
    extension!(
        aux_watermark,
        linux_perf::PERF_ATTR_AUX_WATERMARK_OFFSET,
        u32
    );
    extension!(sample_max_stack, 108, u16);
    extension!(reserved_2, 110, u16);
    extension!(aux_sample_size, 112, u32);
    extension!(aux_action, 116, u32);
    extension!(sig_data, linux_perf::PERF_ATTR_SIG_DATA_OFFSET, u64);
    extension!(config3, linux_perf::PERF_ATTR_CONFIG3_OFFSET, u64);
    extension!(config4, linux_perf::PERF_ATTR_CONFIG4_OFFSET, u64);
    Ok(out)
}

/// Reads only the ABI's size word before deciding whether the complete v0
/// prefix may be copied.  This lets a short, valid mapping report an invalid
/// size instead of spuriously faulting while reading unrelated v0 fields.
pub(crate) fn read_attr_size(
    memory: &UserMemoryCapability,
    attr: *const PerfEventAttr,
) -> AxResult<u32> {
    let address = attr_size_address(attr)?;
    memory
        .read_value_uninit(address as *const u32)
        .map_err(map_usercopy_error)
        .map(|value| unsafe { value.assume_init() })
}

fn attr_size_address(attr: *const PerfEventAttr) -> AxResult<usize> {
    if attr.is_null() {
        return Err(AxError::BadAddress);
    }
    (attr as usize)
        .checked_add(PERF_ATTR_SIZE_OFFSET)
        .ok_or(AxError::BadAddress)
}

/// Linux's E2BIG size probe best-effort reports the largest prefix this
/// kernel understands. Failure to publish that hint does not replace E2BIG.
fn report_supported_attr_size(memory: &UserMemoryCapability, attr: *const PerfEventAttr) {
    let Ok(address) = attr_size_address(attr) else {
        return;
    };
    let _ = memory.write_value(address as *mut u32, linux_perf::PERF_ATTR_SIZE_VER9);
}

/// Converts the first size snapshot into the exact userspace range that may
/// be read. A zero size preserves Linux's v0 compatibility convention.
pub(crate) fn attr_copy_len(size: u32) -> AxResult<usize> {
    let size = if size == 0 { PERF_ATTR_SIZE_VER0 } else { size };
    if size < PERF_ATTR_SIZE_VER0 {
        return Err(AxError::ArgumentListTooLong);
    }
    if size > PERF_ATTR_MAX_SIZE {
        return Err(AxError::ArgumentListTooLong);
    }
    Ok(size as usize)
}

fn validate_extension_bytes(bytes: &[u8]) -> AxResult<()> {
    if bytes.iter().any(|&byte| byte != 0) {
        Err(AxError::ArgumentListTooLong)
    } else {
        Ok(())
    }
}

/// Copies the caller-supplied extension *after* the v9 prefix.  Linux permits
/// an oversized attr only when that tail is all zero and reports the largest
/// prefix it supports through `attr.size` on E2BIG.
pub(crate) fn read_attr_tail(
    memory: &UserMemoryCapability,
    attr: *const PerfEventAttr,
    copy_len: usize,
) -> AxResult<()> {
    let extension_len = copy_len.saturating_sub(linux_perf::PERF_ATTR_SIZE_VER9 as usize);
    let mut extension = [0u8; PERF_ATTR_EXTENSION_CHUNK_SIZE];
    if extension_len == 0 {
        return Ok(());
    }
    // The ABI planner only needs to distinguish zero/non-zero in this tail;
    // consuming it in chunks keeps the syscall bounded and preserves an EFAULT
    // for every declared byte.
    let extension_start = (attr as usize)
        .checked_add(linux_perf::PERF_ATTR_SIZE_VER9 as usize)
        .ok_or(AxError::BadAddress)?;
    let mut offset = 0;
    while offset < extension_len {
        let chunk_len = (extension_len - offset).min(extension.len());
        let address = extension_start
            .checked_add(offset)
            .ok_or(AxError::BadAddress)?;
        memory
            .read_bytes(
                address,
                // This local is initialized by usercopy before inspection.
                unsafe {
                    core::slice::from_raw_parts_mut(
                        extension.as_mut_ptr().cast::<core::mem::MaybeUninit<u8>>(),
                        chunk_len,
                    )
                },
            )
            .map_err(map_usercopy_error)?;
        if let Err(error) = validate_extension_bytes(&extension[..chunk_len]) {
            report_supported_attr_size(memory, attr);
            return Err(error);
        }
        offset += chunk_len;
    }
    Ok(())
}

/// Implements the ABI-valid software-clock subset.  Unsupported perf types
/// fail at creation, rather than producing a descriptor whose samples lie.
pub(crate) fn sys_perf_event_open(
    memory: UserMemoryCapability,
    attr: *const PerfEventAttr,
    pid: i32,
    cpu: i32,
    group_fd: i32,
    flags: u64,
) -> AxResult<isize> {
    let attr_size = read_attr_size(&memory, attr)?;
    let copy_len = match attr_copy_len(attr_size) {
        Ok(copy_len) => copy_len,
        Err(error) => {
            report_supported_attr_size(&memory, attr);
            return Err(error);
        }
    };
    let attr_value = read_attr(
        &memory,
        attr,
        copy_len.min(linux_perf::PERF_ATTR_SIZE_VER9 as usize),
    )?;
    read_attr_tail(&memory, attr, copy_len)?;
    let attr = attr_value;
    // Preserve invalid target syntax ahead of the authorization result while
    // running the security gate before implementation planning.  The latter
    // prevents an unsupported source from acting as a privilege oracle.
    if pid < -1 || (pid == -1 && cpu == -1) {
        return Err(AxError::InvalidInput);
    }
    if flags & PERF_FLAG_FD_OUTPUT != 0 && group_fd < 0 {
        return Err(AxError::InvalidInput);
    }
    let perf_target = if flags & PERF_FLAG_PID_CGROUP != 0 {
        PerfTarget::Cgroup { fd: pid, cpu }
    } else if pid == -1 {
        PerfTarget::Cpu { cpu }
    } else {
        PerfTarget::Task { pid, cpu }
    };
    let target = PerfOpenTarget {
        target: perf_target,
        group_fd: if flags & PERF_FLAG_FD_OUTPUT != 0 {
            -1
        } else {
            group_fd
        },
        output_fd: if flags & PERF_FLAG_FD_OUTPUT != 0 {
            group_fd
        } else {
            -1
        },
        open_flags: flags,
    };
    let authority = PerfAuthority::current();
    authorize_open(authority, &attr, pid, cpu, flags)?;
    authorize_sampling_rate(&attr)?;
    // Parse and validate the complete ABI before consulting a hardware AUX
    // backend.  An invalid extension must not be obscured by a capability
    // result from the local PMU.
    let (_attr_plan, plan) = perf_plan(attr, attr_size, &[], target)?;
    #[cfg(feature = "pmu")]
    let registry_dynamic = dynamic_pmu(attr.event_type);
    let aux = crate::file::perf_aux::AuxRequest::from_attr(&attr, attr_size);
    if let Some(request) = aux {
        // Backend admission remains before descriptor allocation.
        request.admit()?;
    }
    #[cfg(feature = "perf-sampling")]
    if plan.sample.is_some() {
        return open_sampling(
            plan,
            attr,
            target,
            authority,
            &memory,
            (attr.flags & linux_perf::ATTR_FREQ != 0).then_some(attr.sample_period),
            (flags & PERF_FLAG_FD_OUTPUT != 0).then_some(group_fd),
            aux,
        );
    }
    #[cfg(feature = "pmu")]
    if matches!(
        registry_dynamic,
        Some(DynamicPmu::Uncore { .. } | DynamicPmu::ReadOnly(_))
    ) && (pid != -1 || cpu < 0)
    {
        // Uncore is system-wide/per-CPU only. The group may be scheduled on
        // any advertised package CPU; its bounded transport sends each
        // selector/read/restore transaction to the discovered package owner.
        return Err(AxError::OperationNotSupported);
    }
    if flags & PERF_FLAG_PID_CGROUP != 0 {
        if cpu < 0 || cpu as usize >= axhal::cpu_num() {
            return Err(AxError::InvalidInput);
        }
        let cgroup_id = crate::pseudofs::cgroup::perf_cgroup_fd_identity(pid)?;
        let context = crate::file::perf::PerfContext::Cgroup {
            cgroup_id,
            cpu: cpu as usize,
        };
        let raw_core_type = raw_core_type_for_event(target, attr.event_type)?;
        #[cfg(feature = "pmu")]
        let mut lowered = match registry_dynamic {
            Some(source @ (DynamicPmu::Kprobe | DynamicPmu::Uprobe)) => {
                dynamic_probe_event(source, &attr, &memory, current().id().as_u64())?
                    .ok_or(AxError::InvalidInput)?
            }
            Some(source) => LoweredPerfEvent::unnamed(dynamic_pmu_event(
                source,
                attr.config,
                precise_ip_level(&attr) == 1,
                attr.branch_sample_type != 0,
            )?),
            None => systemwide_event_with_uprobe(plan.event, raw_core_type, &memory)?,
        };
        #[cfg(not(feature = "pmu"))]
        let mut lowered = systemwide_event_with_uprobe(plan.event, raw_core_type, &memory)?;
        let event = lowered.event;
        let id = NEXT_PERF_EVENT_ID.fetch_add(1, Ordering::Relaxed);
        let (group, attached_cpu_context) = if group_fd == -1 {
            let group = PerfGroup::new_for_context(context, id)?;
            (group, true)
        } else {
            if flags & PERF_FLAG_FD_NO_GROUP != 0 {
                return Err(AxError::OperationNotSupported);
            }
            let leader = get_typed_file::<PerfEventFile>(group_fd)?;
            let group = leader.group().ok_or(AxError::BadFileDescriptor)?;
            if !leader.is_group_leader() || group.context() != context {
                return Err(AxError::InvalidInput);
            }
            (group, false)
        };
        let file = match PerfEventFile::new_with_lifecycle_placement_domains(
            id,
            event,
            plan.disabled,
            &group,
            plan.read,
            plan.lifecycle,
            placement_policy(&attr),
            !plan.exclude_user,
            !plan.exclude_kernel,
        ) {
            Ok(file) => file,
            Err(error) => {
                if attached_cpu_context {
                    PerfGroup::detach_empty_cpu_context(&group);
                }
                return Err(error);
            }
        };
        install_probe_query_name(&file, &mut lowered);
        if attached_cpu_context {
            PerfGroup::attach_cpu_context(&group)?;
        }
        return match add_file_like(file as Arc<dyn crate::file::FileLike>, plan.close_on_exec) {
            Ok(fd) => Ok(fd as isize),
            Err(error) => {
                if attached_cpu_context {
                    PerfGroup::detach_empty_cpu_context(&group);
                }
                Err(error)
            }
        };
    }
    if pid == -1 {
        if cpu < 0 || cpu as usize >= axhal::cpu_num() {
            return Err(AxError::InvalidInput);
        }
        let context = crate::file::perf::PerfContext::Cpu { cpu: cpu as usize };
        let raw_core_type = raw_core_type_for_event(target, attr.event_type)?;
        #[cfg(feature = "pmu")]
        let mut lowered = match registry_dynamic {
            Some(source @ (DynamicPmu::Kprobe | DynamicPmu::Uprobe)) => {
                dynamic_probe_event(source, &attr, &memory, current().id().as_u64())?
                    .ok_or(AxError::InvalidInput)?
            }
            Some(source) => LoweredPerfEvent::unnamed(dynamic_pmu_event(
                source,
                attr.config,
                precise_ip_level(&attr) == 1,
                attr.branch_sample_type != 0,
            )?),
            None => systemwide_event_with_uprobe(plan.event, raw_core_type, &memory)?,
        };
        #[cfg(not(feature = "pmu"))]
        let mut lowered = systemwide_event_with_uprobe(plan.event, raw_core_type, &memory)?;
        let event = lowered.event;
        let id = NEXT_PERF_EVENT_ID.fetch_add(1, Ordering::Relaxed);
        let (group, attached_cpu_context) = if group_fd == -1 {
            let group = PerfGroup::new_for_context(context, id)?;
            (group, true)
        } else {
            if flags & PERF_FLAG_FD_NO_GROUP != 0 {
                return Err(AxError::OperationNotSupported);
            }
            let leader = get_typed_file::<PerfEventFile>(group_fd)?;
            let group = leader.group().ok_or(AxError::BadFileDescriptor)?;
            if !leader.is_group_leader() || group.context() != context {
                return Err(AxError::InvalidInput);
            }
            (group, false)
        };
        let file = match PerfEventFile::new_with_lifecycle_placement_domains(
            id,
            event,
            plan.disabled,
            &group,
            plan.read,
            plan.lifecycle,
            placement_policy(&attr),
            !plan.exclude_user,
            !plan.exclude_kernel,
        ) {
            Ok(file) => file,
            Err(error) => {
                if attached_cpu_context {
                    PerfGroup::detach_empty_cpu_context(&group);
                }
                return Err(error);
            }
        };
        install_probe_query_name(&file, &mut lowered);
        if attached_cpu_context {
            PerfGroup::attach_cpu_context(&group)?;
        }
        return match add_file_like(file as Arc<dyn crate::file::FileLike>, plan.close_on_exec) {
            Ok(fd) => Ok(fd as isize),
            Err(error) => {
                if attached_cpu_context {
                    PerfGroup::detach_empty_cpu_context(&group);
                }
                Err(error)
            }
        };
    }
    // `-1` is the task-any-CPU selector.  Check the upper bound only after
    // excluding that sentinel; casting it to usize would otherwise turn a
    // valid task event into EINVAL.
    if cpu < -1 || (cpu >= 0 && cpu as usize >= axhal::cpu_num()) {
        return Err(AxError::InvalidInput);
    }
    let target_is_current = pid == 0;
    let target_task = if target_is_current {
        current().clone()
    } else {
        let tid = current()
            .as_thread()
            .pid_ns()
            .resolve_visible_pid(pid as u32)
            .ok_or(AxError::NoSuchProcess)?;
        get_visible_task(tid)?
    };
    // perf's task attachment has ptrace-style credential access semantics.
    check_current_thread_ptrace_image_access(target_task.as_thread(), PtraceAccessMode::ReadReal)?;
    let target_task_id = target_task.id().as_u64();
    let context = if cpu == -1 {
        crate::file::perf::PerfContext::Task {
            task_id: target_task_id,
        }
    } else {
        crate::file::perf::PerfContext::TaskOnCpu {
            task_id: target_task_id,
            cpu: cpu as usize,
        }
    };
    let raw_core_type = raw_core_type_for_event(target, attr.event_type)?;
    let (id, group) = if group_fd == -1 {
        // Reserve the ID before group construction so the group's immutable
        // leader identity and the created event cannot diverge.
        let id = NEXT_PERF_EVENT_ID.fetch_add(1, Ordering::Relaxed);
        (id, PerfGroup::new_for_context(context, id)?)
    } else {
        if flags & PERF_FLAG_FD_NO_GROUP != 0 {
            return Err(AxError::OperationNotSupported);
        }
        let leader = get_typed_file::<PerfEventFile>(group_fd)?;
        let Some(group) = leader.group() else {
            return Err(AxError::BadFileDescriptor);
        };
        if !leader.is_group_leader() || group.context() != context {
            return Err(AxError::InvalidInput);
        }
        (NEXT_PERF_EVENT_ID.fetch_add(1, Ordering::Relaxed), group)
    };
    let mut probe_name = None;
    #[cfg(feature = "pmu")]
    let event = if let Some(source) = registry_dynamic {
        if let Some(lowered) = dynamic_probe_event(source, &attr, &memory, target_task_id)? {
            probe_name = lowered.probe_name;
            lowered.event
        } else {
            dynamic_pmu_event(
                source,
                attr.config,
                precise_ip_level(&attr) == 1,
                attr.branch_sample_type != 0,
            )?
        }
    } else {
        match plan.event {
            linux_perf::Event::SoftwareCpuClock => PerfEvent::Software(SoftwareEvent::CpuClock),
            linux_perf::Event::SoftwareTaskClock => PerfEvent::Software(SoftwareEvent::TaskClock),
            linux_perf::Event::SoftwarePageFaults => PerfEvent::Software(SoftwareEvent::PageFaults),
            linux_perf::Event::SoftwarePageFaultsMin => {
                PerfEvent::Software(SoftwareEvent::PageFaultsMin)
            }
            linux_perf::Event::SoftwarePageFaultsMaj => {
                PerfEvent::Software(SoftwareEvent::PageFaultsMaj)
            }
            linux_perf::Event::SoftwareCpuMigrations => {
                PerfEvent::Software(SoftwareEvent::CpuMigrations)
            }
            linux_perf::Event::SoftwareContextSwitches => {
                PerfEvent::Software(SoftwareEvent::ContextSwitches)
            }
            linux_perf::Event::SoftwareAlignmentFaults => {
                PerfEvent::Software(SoftwareEvent::AlignmentFaults)
            }
            linux_perf::Event::SoftwareEmulationFaults => {
                PerfEvent::Software(SoftwareEvent::EmulationFaults)
            }
            linux_perf::Event::SoftwareDummy => PerfEvent::Software(SoftwareEvent::Dummy),
            linux_perf::Event::SoftwareCgroupSwitches => {
                PerfEvent::Software(SoftwareEvent::CgroupSwitches)
            }
            linux_perf::Event::SoftwareBpfOutput => PerfEvent::Software(SoftwareEvent::BpfOutput),
            linux_perf::Event::HardwareCycles | linux_perf::Event::HardwareInstructions => {
                #[cfg(not(feature = "pmu"))]
                return Err(AxError::OperationNotSupported);
                #[cfg(feature = "pmu")]
                {
                    if axhal::pmu::capabilities().is_err() {
                        return Err(AxError::OperationNotSupported);
                    }
                    PerfEvent::Hardware(match plan.event {
                        linux_perf::Event::HardwareCycles => HardwareEvent::Cycles,
                        linux_perf::Event::HardwareInstructions => HardwareEvent::Instructions,
                        _ => unreachable!(),
                    })
                }
            }
            event if architectural_hardware_event(event).is_some() => {
                #[cfg(not(feature = "pmu"))]
                return Err(AxError::OperationNotSupported);
                #[cfg(feature = "pmu")]
                {
                    axhal::pmu::capabilities().map_err(|_| AxError::OperationNotSupported)?;
                    let hardware = architectural_hardware_event(event).unwrap();
                    let HardwareEvent::Architectural {
                        availability_bit, ..
                    } = hardware
                    else {
                        unreachable!()
                    };
                    axhal::pmu::architectural_event_supported_fleet(availability_bit)
                        .map_err(|_| AxError::OperationNotSupported)?;
                    PerfEvent::Hardware(hardware)
                }
            }
            linux_perf::Event::Tracepoint(id) => {
                // Do not turn an arbitrary numeric ID into a no-op. The source
                // registry owns the exact set of tracefs hooks emitted by the
                // scheduler/MM paths.
                crate::perf_sources::tracepoint(id)?;
                PerfEvent::Tracepoint(id)
            }
            linux_perf::Event::Raw(config) => PerfEvent::Raw {
                config,
                core_type: raw_core_type.ok_or(AxError::OperationNotSupported)?,
                precise: precise_ip_level(&attr) == 1,
                branch_stack: attr.branch_sample_type != 0,
            },
            linux_perf::Event::Breakpoint { addr, len, ty } => {
                PerfEvent::Breakpoint { addr, len, ty }
            }
            linux_perf::Event::Kprobe { function, offset } => {
                let addr = function.checked_add(offset).ok_or(AxError::InvalidInput)?;
                PerfEvent::Kprobe {
                    addr,
                    retprobe: false,
                    query_offset: 0,
                }
            }
            linux_perf::Event::Uprobe {
                path,
                offset,
                retprobe,
            } => {
                let file = crate::perf_sources::resolve_uprobe_inode(
                    &memory,
                    path as *const u8,
                    target_task_id,
                )?;
                probe_name = Some(file.name);
                PerfEvent::Uprobe {
                    mount_id: file.key.mount_id,
                    device: file.key.device,
                    inode: file.key.inode,
                    offset,
                    retprobe,
                    reference_counter_offset: 0,
                }
            }
            // This task path only admits the explicitly lowered PMU/cache
            // forms above; guarded match arms do not cover the remainder.
            _ => return Err(AxError::OperationNotSupported),
        }
    };
    #[cfg(not(feature = "pmu"))]
    let event = {
        let lowered = systemwide_event_with_uprobe(plan.event, raw_core_type, &memory)?;
        probe_name = lowered.probe_name;
        lowered.event
    };
    // Sampling, output routing and read-format extensions are not fabricated.
    if !matches!(event, PerfEvent::Software(SoftwareEvent::BpfOutput))
        && (attr.sample_period != 0 || attr.sample_type != 0)
    {
        return Err(AxError::OperationNotSupported);
    }
    let file = match PerfEventFile::new_with_lifecycle_placement_domains(
        id,
        event,
        plan.disabled,
        &group,
        plan.read,
        plan.lifecycle,
        placement_policy(&attr),
        !plan.exclude_user,
        !plan.exclude_kernel,
    ) {
        Ok(file) => file,
        Err(error) => {
            target_task.as_thread().detach_empty_perf_group(&group);
            return Err(error);
        }
    };
    if let Some(name) = probe_name {
        file.install_probe_query_name(name);
    }
    if let PerfEvent::Uprobe {
        mount_id,
        device,
        inode,
        offset,
        retprobe,
        reference_counter_offset,
    } = event
    {
        // Registration and the private COW INT3 installation are one open
        // transaction: on failure dropping `file` releases the consumer and
        // restores any already-published overlay bytes.
        if let Err(error) = crate::uprobe::install_for_mm(
            &target_task.as_thread().proc_data.aspace(),
            crate::uprobe::UprobeFileKey {
                mount_id,
                device,
                inode,
            },
            offset,
            retprobe,
            reference_counter_offset,
        ) {
            drop(file);
            target_task.as_thread().detach_empty_perf_group(&group);
            return Err(error);
        }
    }
    // The live member prevents a scheduler edge from pruning the group
    // between registry publication and descriptor installation.
    target_task.as_thread().attach_perf_group(group.clone())?;
    let result = add_file_like(file as Arc<dyn crate::file::FileLike>, plan.close_on_exec);
    match result {
        Ok(fd) => {
            if target_is_current {
                group.reconfigure_current();
                current().as_thread().refresh_perf_debug_registers();
            }
            Ok(fd as isize)
        }
        Err(error) => {
            target_task.as_thread().detach_empty_perf_group(&group);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use axerrno::AxError;

    use super::{PERF_ATTR_MAX_SIZE, PERF_ATTR_SIZE_VER0, attr_copy_len, validate_extension_bytes};
    use crate::file::PerfGroup;

    #[cfg(feature = "perf-sampling")]
    #[test]
    fn scheduler_tracepoint_can_sample_every_edge_with_monotonic_time() {
        let _context = crate::test_support::scheduler_test_context();
        let attr = super::PerfEventAttr {
            event_type: thekernel_linux_perf::PERF_TYPE_TRACEPOINT,
            config: crate::perf_sources::SCHED_WAKEUP_TRACEPOINT_ID,
            read_format: thekernel_linux_perf::PERF_FORMAT_LOST,
            sample_period: 1,
            sample_type: thekernel_linux_perf::PERF_SAMPLE_TIME | thekernel_linux_perf::PERF_SAMPLE_RAW,
            flags: thekernel_linux_perf::ATTR_USE_CLOCKID,
            clockid: linux_raw_sys::general::CLOCK_MONOTONIC as i32,
            ..super::PerfEventAttr::default()
        };
        let target = super::PerfOpenTarget {
            target: super::PerfTarget::Cpu { cpu: 0 },
            group_fd: -1,
            output_fd: -1,
            open_flags: 0,
        };
        let (_, plan) = super::perf_plan(attr, thekernel_linux_perf::PERF_ATTR_SIZE_VER3, &[], target).unwrap();
        assert_eq!(plan.sample.unwrap().period, 1);
        // The collector uses byte-watermark wakeups; zero event-count wakeups
        // above remain a separate valid Linux configuration.
        let collector = super::PerfEventAttr {
            flags: attr.flags | thekernel_linux_perf::ATTR_WATERMARK,
            wakeup_events: 64 * 4096 / 2,
            ..attr
        };
        let (schema, _) = super::perf_plan(collector, thekernel_linux_perf::PERF_ATTR_SIZE_VER9, &[], target).unwrap();
        assert_eq!(schema.wakeup, thekernel_linux_perf::Wakeup::Watermark(131072));
    }

    #[test]
    fn perf_clock_selection_accepts_only_the_monotonic_backend_clock() {
        let _context = crate::test_support::scheduler_test_context();
        let target = super::PerfOpenTarget {
            target: super::PerfTarget::Task { pid: 0, cpu: -1 },
            group_fd: -1,
            output_fd: -1,
            open_flags: 0,
        };
        let mut attr = super::PerfEventAttr {
            event_type: thekernel_linux_perf::PERF_TYPE_SOFTWARE,
            config: thekernel_linux_perf::PERF_COUNT_SW_CPU_CLOCK,
            flags: thekernel_linux_perf::ATTR_USE_CLOCKID,
            clockid: linux_raw_sys::general::CLOCK_MONOTONIC as i32,
            ..super::PerfEventAttr::default()
        };
        assert!(super::perf_plan(attr, thekernel_linux_perf::PERF_ATTR_SIZE_VER3, &[], target).is_ok());
        for clockid in [-1, 0, 2, 4, 7] {
            attr.clockid = clockid;
            assert!(matches!(
                super::perf_plan(attr, thekernel_linux_perf::PERF_ATTR_SIZE_VER3, &[], target),
                Err(AxError::OperationNotSupported),
            ));
        }
        attr.flags = 0;
        assert!(super::perf_plan(attr, thekernel_linux_perf::PERF_ATTR_SIZE_VER3, &[], target).is_ok());
    }

    #[test]
    fn perf_group_binds_leader_and_target_task() {
        let group = PerfGroup::new(41, 7).unwrap();
        assert!(group.is_group_leader_for_test(7));
        assert!(!group.is_group_leader_for_test(8));
        assert!(group.accepts_target(41));
        assert!(!group.accepts_target(42));
    }

    #[test]
    fn perf_attr_extension_validator_accepts_only_zero_tail_with_bounded_size() {
        // A zero word and a full v0 word both authorize exactly the v0 copy;
        // a short partial mapping is rejected before that copy is attempted.
        assert_eq!(attr_copy_len(0).unwrap(), PERF_ATTR_SIZE_VER0 as usize);
        assert_eq!(
            attr_copy_len(PERF_ATTR_SIZE_VER0),
            Ok(PERF_ATTR_SIZE_VER0 as usize)
        );
        assert_eq!(
            attr_copy_len(PERF_ATTR_SIZE_VER0 - 1),
            Err(AxError::ArgumentListTooLong)
        );
        assert_eq!(attr_copy_len(PERF_ATTR_MAX_SIZE).unwrap(), 4096);
        assert_eq!(
            attr_copy_len(PERF_ATTR_MAX_SIZE + 1),
            Err(AxError::ArgumentListTooLong)
        );

        assert_eq!(validate_extension_bytes(&[0; 3]), Ok(()));
        assert_eq!(
            validate_extension_bytes(&[0, 0, 1]),
            Err(AxError::ArgumentListTooLong)
        );
    }

    #[cfg(feature = "pmu")]
    #[test]
    fn task_any_cpu_keeps_the_negative_one_selector_out_of_cpu_bounds_checks() {
        assert!(super::readonly_target_is_owned(
            thekernel_linux_perf::PerfTarget::Cpu { cpu: 3 },
            3,
        ));
        assert!(!super::readonly_target_is_owned(
            thekernel_linux_perf::PerfTarget::Task { pid: 1, cpu: -1 },
            3,
        ));
    }

    #[cfg(feature = "perf-sampling")]
    #[test]
    fn source_sampling_never_accepts_pebs_or_lbr_only_fields() {
        use thekernel_linux_perf::{
            PERF_SAMPLE_ADDR, PERF_SAMPLE_BRANCH_STACK, PERF_SAMPLE_DATA_SRC, PerfEventAttr,
        };

        // ATTR_PRECISE_IP is the two-bit field mask (levels 1..3), while the
        // backend accepts only the level-1 PEBS guarantee.
        const PRECISE_IP_LEVEL1: u64 = 1 << 15;

        let source = crate::file::perf_sampling::SamplingEvent::Source;
        let hardware = crate::file::perf_sampling::SamplingEvent::Cycles;
        for sample_type in [
            PERF_SAMPLE_ADDR,
            PERF_SAMPLE_DATA_SRC,
            PERF_SAMPLE_BRANCH_STACK,
        ] {
            let attr = PerfEventAttr {
                sample_type,
                ..PerfEventAttr::default()
            };
            assert!(!super::sampling_fields_supported_by_backend(&attr, source));
        }
        let precise = PerfEventAttr {
            flags: PRECISE_IP_LEVEL1,
            ..PerfEventAttr::default()
        };
        assert!(!super::sampling_fields_supported_by_backend(
            &precise, source
        ));

        let pebs = PerfEventAttr {
            flags: PRECISE_IP_LEVEL1,
            sample_type: PERF_SAMPLE_ADDR | PERF_SAMPLE_DATA_SRC,
            ..PerfEventAttr::default()
        };
        assert!(super::sampling_fields_supported_by_backend(&pebs, hardware));
        let overprecise = PerfEventAttr {
            flags: thekernel_linux_perf::ATTR_PRECISE_IP,
            sample_type: PERF_SAMPLE_ADDR | PERF_SAMPLE_DATA_SRC,
            ..PerfEventAttr::default()
        };
        assert!(!super::sampling_fields_supported_by_backend(
            &overprecise, hardware
        ));
        let inexact = PerfEventAttr {
            sample_type: PERF_SAMPLE_ADDR | PERF_SAMPLE_DATA_SRC,
            ..PerfEventAttr::default()
        };
        assert!(!super::sampling_fields_supported_by_backend(
            &inexact, hardware
        ));
    }
}
