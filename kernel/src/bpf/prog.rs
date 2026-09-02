//! BPF program storage and metadata.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use axerrno::{AxError, AxResult};
use axsync::spin::SpinNoIrq;
use thekernel_linux_bpf::ProgramProfile;

use super::{defs::*, map::BpfMap};

/// A Linux file-descriptor map reference retained by a loaded program.
/// AXBPF carries the reference in bytecode; the kernel owns the object
/// lifetime needed to turn it into a map capability at execution time.
#[derive(Clone)]
pub struct BpfMapBinding {
    pub reference: axbpf::MapRef,
    pub map: Arc<dyn BpfMap>,
    /// Keeps the map's memlock reservation live when a program is its last
    /// kernel reference after userspace closes the original map FD.
    pub memory_charge: Arc<crate::bpf_security::BpfMemoryCharge>,
}

/// A map retained by `BPF_PROG_BIND_MAP` without becoming an instruction
/// operand.  Linux uses this association to couple object lifetimes (for
/// example for global-data and loader-owned maps); execution continues to use
/// only the verifier-resolved `maps` array above.
pub struct BpfBoundMap {
    pub map: Arc<dyn BpfMap>,
    pub memory_charge: Arc<crate::bpf_security::BpfMemoryCharge>,
}

const BPF_STDOUT: u32 = 1;
const BPF_STDERR: u32 = 2;
const BPF_STREAM_MAX_CAPACITY: usize = 100_000;

struct BpfStreamElement {
    bytes: Vec<u8>,
    consumed: usize,
}

/// FIFO byte stream owned by one loaded program.  Elements stay distinct so
/// producers can publish an entire staged diagnostic atomically, while reads
/// may consume an element partially just like Linux's standard streams.
pub struct BpfStreamState {
    backlog: VecDeque<BpfStreamElement>,
    capacity: usize,
}

impl BpfStreamState {
    pub const fn new() -> Self {
        Self {
            backlog: VecDeque::new(),
            capacity: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> AxResult<()> {
        let capacity = self
            .capacity
            .checked_add(bytes.len())
            .ok_or(AxError::StorageFull)?;
        if capacity > BPF_STREAM_MAX_CAPACITY {
            return Err(AxError::StorageFull);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(bytes.len())
            .map_err(|_| AxError::NoMemory)?;
        owned.extend_from_slice(bytes);
        self.backlog.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        self.backlog.push_back(BpfStreamElement {
            bytes: owned,
            consumed: 0,
        });
        self.capacity = capacity;
        Ok(())
    }

    pub fn snapshot(&self, maximum: usize) -> AxResult<Vec<u8>> {
        let wanted = core::cmp::min(maximum, self.capacity);
        let mut output = Vec::new();
        output
            .try_reserve_exact(wanted)
            .map_err(|_| AxError::NoMemory)?;
        for element in &self.backlog {
            if output.len() == wanted {
                break;
            }
            let available = &element.bytes[element.consumed..];
            let take = core::cmp::min(available.len(), wanted - output.len());
            output.extend_from_slice(&available[..take]);
        }
        Ok(output)
    }

    pub fn consume(&mut self, mut bytes: usize) {
        self.capacity = self.capacity.saturating_sub(bytes);
        while bytes != 0 {
            let Some(front) = self.backlog.front_mut() else {
                break;
            };
            let available = front.bytes.len() - front.consumed;
            if bytes < available {
                front.consumed += bytes;
                break;
            }
            bytes -= available;
            self.backlog.pop_front();
        }
    }
}

pub fn uses_raw_ctx_prog_type(prog_type: u32) -> bool {
    matches!(
        ProgramProfile::try_from(prog_type),
        Ok(ProgramProfile::SocketFilter
            | ProgramProfile::Tracepoint
            | ProgramProfile::PerfEvent
            | ProgramProfile::RawTracepoint)
    ) || matches!(
        prog_type,
        BPF_PROG_TYPE_RAW_TRACEPOINT_WRITABLE | BPF_PROG_TYPE_TRACING | BPF_PROG_TYPE_STRUCT_OPS
    ) || matches!(
        prog_type,
        BPF_PROG_TYPE_CGROUP_SKB | BPF_PROG_TYPE_NETFILTER | BPF_PROG_TYPE_XDP
    )
}

/// Linux attachment profiles which are intentionally kernel-owned rather than
/// part of the portable BPF policy enum.
pub const BPF_PROG_TYPE_TRACING: u32 = 26;
pub const BPF_PROG_TYPE_STRUCT_OPS: u32 = 27;
/// Linux BPF LSM programs execute at typed security-dispatch boundaries.
pub const BPF_PROG_TYPE_LSM: u32 = 29;
/// Attachment kinds owned by the tracing/LSM link layer.
pub const BPF_TRACE_RAW_TP: u32 = 23;
pub const BPF_TRACE_FENTRY: u32 = 24;
pub const BPF_TRACE_FEXIT: u32 = 25;
pub const BPF_MODIFY_RETURN: u32 = 26;
pub const BPF_LSM_MAC: u32 = 27;
pub const BPF_TRACE_ITER: u32 = 28;

/// Attachment points implemented by the namespace packet pipeline.  Keep the
/// numbers here with the Linux-program ownership code: they are part of the
/// `expected_attach_type` contract, not a transport-private socket option.
pub const BPF_CGROUP_INET_INGRESS: u32 = 0;
pub const BPF_CGROUP_INET_EGRESS: u32 = 1;
pub const BPF_NETFILTER: u32 = 47;
/// `enum bpf_attach_type::BPF_XDP`.  XDP link targets are interface indices
/// in the caller's network namespace, not descriptor numbers.
pub const BPF_XDP: u32 = 37;

pub const fn is_network_attach_type(attach_type: u32) -> bool {
    matches!(
        attach_type,
        BPF_CGROUP_INET_INGRESS | BPF_CGROUP_INET_EGRESS | BPF_NETFILTER | BPF_XDP
    )
}

/// A loaded (and verified) BPF program.
pub struct BpfProgram {
    /// Canonical generic program mechanism. Linux FD metadata and map handles
    /// remain alongside it because they are kernel resources.
    pub mechanism: axbpf::Program,
    pub prog_type: u32,
    pub name: [u8; BPF_OBJ_NAME_LEN],
    pub prog_id: u32,
    pub expected_attach_type: u32,
    /// BTF hook identity for tracing/LSM programs.  It is retained with the
    /// verified program rather than resampled from a recycled numeric BTF FD
    /// at link creation.
    pub attach_btf_id: u32,
    /// Maps referenced by this program (resolved during verification).
    pub maps: Vec<BpfMapBinding>,
    /// Additional lifetime-only map bindings installed after program load.
    /// The lock makes duplicate detection and publication one transaction.
    pub bound_maps: SpinNoIrq<Vec<BpfBoundMap>>,
    pub streams: [axsync::Mutex<BpfStreamState>; 2],
    /// GPL-compatible license.
    pub gpl_compatible: bool,
    /// Program text and binding charge, retained by every attachment rather
    /// than merely by the originating descriptor.
    pub memory_charge: Arc<crate::bpf_security::BpfMemoryCharge>,
    pub run_time_ns: AtomicU64,
    pub run_cnt: AtomicU64,
}

pub struct BpfStatsRunGuard {
    enabled: bool,
    started: u64,
}
impl BpfStatsRunGuard {
    pub fn begin() -> Self {
        Self {
            enabled: crate::file::bpf::bpf_run_time_stats_enabled(),
            started: axhal::time::monotonic_time_nanos(),
        }
    }
    pub fn elapsed(&self) -> u64 {
        axhal::time::monotonic_time_nanos().saturating_sub(self.started)
    }
}
impl BpfProgram {
    fn stream_index(stream_id: u32) -> AxResult<usize> {
        match stream_id {
            BPF_STDOUT => Ok(0),
            BPF_STDERR => Ok(1),
            _ => Err(AxError::NotFound),
        }
    }

    pub fn stream(&self, stream_id: u32) -> AxResult<axsync::MutexGuard<'_, BpfStreamState>> {
        Ok(self.streams[Self::stream_index(stream_id)?].lock())
    }

    pub fn push_stream(&self, stream_id: u32, bytes: &[u8]) -> AxResult<()> {
        self.stream(stream_id)?.push(bytes)
    }

    pub fn bind_map(
        &self,
        map: Arc<dyn BpfMap>,
        memory_charge: Arc<crate::bpf_security::BpfMemoryCharge>,
    ) -> AxResult<()> {
        let mut bound = self.bound_maps.lock();
        if self
            .maps
            .iter()
            .any(|candidate| Arc::ptr_eq(&candidate.map, &map))
            || bound
                .iter()
                .any(|candidate| Arc::ptr_eq(&candidate.map, &map))
        {
            return Ok(());
        }
        bound.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        bound.push(BpfBoundMap { map, memory_charge });
        Ok(())
    }

    pub fn account_run(&self, guard: &BpfStatsRunGuard) {
        if guard.enabled {
            self.run_cnt.fetch_add(1, Ordering::Relaxed);
            let elapsed_ns = guard.elapsed();
            let _ = self
                .run_time_ns
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                    Some(old.saturating_add(elapsed_ns))
                });
        }
    }

    /// Run this program with the fixed, kernel-owned XDP context profile.
    /// XDP programs never receive a writable packet or context capability
    /// through this path; redirect decisions retain their typed target in the
    /// returned terminal result instead of leaking an install-time map FD.
    pub(crate) fn run_xdp(
        &self,
        context: crate::bpf::helpers::XdpContext,
        packet: &[u8],
    ) -> AxResult<crate::bpf::helpers::XdpExecutionResult> {
        if self.prog_type != BPF_PROG_TYPE_XDP {
            return Err(AxError::InvalidInput);
        }
        let mut bytes = context.to_bytes();
        let stats = BpfStatsRunGuard::begin();
        let result = crate::bpf::helpers::BpfExecution::new(&mut bytes, &self.maps, 4096)
            .with_streams(&self.streams)
            .execute_xdp(&self.mechanism, packet);
        self.account_run(&stats);
        result.map(|(terminal, _)| terminal)
    }
}
