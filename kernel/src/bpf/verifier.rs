//! Linux policy wrapper around AXBPF verification.
use alloc::{string::String, vec::Vec};
use core::cell::RefCell;

use axerrno::AxError;

use super::{
    defs::*,
    helpers::LinuxHelperPolicy,
    prog::{BpfMapBinding, uses_raw_ctx_prog_type},
};
use crate::file::{FileLike, bpf::BpfMapFd};

struct KernelMapResolver(RefCell<Vec<BpfMapBinding>>);
impl axbpf::MapResolver for KernelMapResolver {
    fn resolve(&self, r: axbpf::MapRef) -> Option<axbpf::MapInfo> {
        self.resolve_fd(r.fd())
    }
    fn resolve_fd(&self, fd: i32) -> Option<axbpf::MapInfo> {
        let mut bindings = self.0.borrow_mut();
        let reference = axbpf::MapRef::from_fd(fd);
        if !bindings
            .iter()
            .any(|binding| binding.reference == reference)
        {
            let map = BpfMapFd::from_fd(fd).ok()?;
            bindings.try_reserve(1).ok()?;
            bindings.push(BpfMapBinding {
                reference,
                map: map.map.clone(),
                memory_charge: map.memory_charge.clone(),
            });
        }
        let binding = bindings
            .iter()
            .find(|binding| binding.reference == reference)?;
        Some(axbpf::MapInfo {
            key_size: binding.map.key_size(),
            value_size: binding.map.value_size(),
            max_entries: binding.map.max_entries(),
        })
    }
}
pub struct VerifiedProgram {
    pub portable: axbpf::Program,
    pub maps: Vec<BpfMapBinding>,
    pub log: String,
}
pub struct VerifierFailure {
    pub err: AxError,
    pub log: String,
}
fn failure(log_level: u32, message: &str) -> VerifierFailure {
    VerifierFailure {
        err: AxError::InvalidInput,
        log: if log_level > 0 {
            String::from(message)
        } else {
            String::new()
        },
    }
}
/// Verifies generic bytecode in AXBPF, then retains only Linux FD resources.
pub fn verify_program(
    insns: &[BpfInsn],
    prog_type: u32,
    log_level: u32,
) -> Result<VerifiedProgram, VerifierFailure> {
    if !uses_raw_ctx_prog_type(prog_type) && prog_type != crate::bpf::prog::BPF_PROG_TYPE_LSM {
        return Err(failure(log_level, "unsupported Linux BPF program type"));
    }
    let perf_event = prog_type == BPF_PROG_TYPE_PERF_EVENT;
    let xdp = prog_type == BPF_PROG_TYPE_XDP;
    let policy = axbpf::VerifyPolicy {
        max_instructions: axbpf::DEFAULT_MAX_INSTRUCTIONS,
        stack_bytes: axbpf::DEFAULT_STACK_BYTES,
        // Raw tracepoint programs are verified against the largest prototype
        // this kernel publishes (four u64 slots).  Link creation then binds
        // the measured requirement to its selected prototype; no producer
        // manufactures a permissive u32::MAX context.
        context_bytes: if perf_event {
            32
        } else if matches!(
            prog_type,
            crate::bpf::defs::BPF_PROG_TYPE_RAW_TRACEPOINT
                | crate::bpf::defs::BPF_PROG_TYPE_RAW_TRACEPOINT_WRITABLE
        ) {
            32
        } else if xdp {
            crate::bpf::helpers::XDP_CONTEXT_BYTES as u32
        } else {
            u32::MAX
        },
        // Raw tracepoints receive read-only argument arrays; only the
        // explicitly writable raw-tracepoint profile may mutate its producer
        // context.  Other legacy packet/tracing profiles retain their own
        // established writable-context contracts.
        context_writable: !perf_event
            && !xdp
            && prog_type != crate::bpf::defs::BPF_PROG_TYPE_RAW_TRACEPOINT,
        context_pointer_fields: if xdp {
            [
                Some(axbpf::ContextPointerField {
                    offset: 0,
                    width: 4,
                    region: axbpf::MemoryRegion::Custom(1),
                    max_length: crate::bpf::helpers::XDP_MAX_PACKET_BYTES,
                    writable: false,
                }),
                Some(axbpf::ContextPointerField {
                    offset: 4,
                    width: 4,
                    region: axbpf::MemoryRegion::Custom(1),
                    max_length: crate::bpf::helpers::XDP_MAX_PACKET_BYTES,
                    writable: false,
                }),
                None,
            ]
        } else if matches!(
            prog_type,
            crate::bpf::defs::BPF_PROG_TYPE_RAW_TRACEPOINT
                | crate::bpf::defs::BPF_PROG_TYPE_RAW_TRACEPOINT_WRITABLE
        ) {
            [
                Some(axbpf::ContextPointerField {
                    offset: 0,
                    width: 8,
                    region: axbpf::MemoryRegion::Custom(2),
                    max_length: axcpu::uspace::LinuxPtRegs::BYTE_LEN as u32,
                    writable: false,
                }),
                None,
                None,
            ]
        } else {
            [None; 3]
        },
        allow_loops: false,
        max_states: axbpf::DEFAULT_MAX_INSTRUCTIONS * 4,
    };
    let helpers = LinuxHelperPolicy {
        allow_perf_event: perf_event,
        allow_xdp_redirect: xdp,
    };
    // Resolve each numeric descriptor once and retain that exact object through
    // verification and execution, even if another thread recycles the FD.
    let resolver = KernelMapResolver(RefCell::new(Vec::new()));
    let mechanism = axbpf::Program::verify(insns, &resolver, &helpers, policy)
        .map_err(|_| failure(log_level, "AXBPF rejected program"))?;
    let maps = resolver.0.into_inner();
    Ok(VerifiedProgram {
        portable: mechanism,
        maps,
        log: String::new(),
    })
}
