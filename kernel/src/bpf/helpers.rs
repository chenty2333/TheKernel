//! Linux eBPF helper policy and AXBPF capability adapter.
use alloc::{rc::Rc, sync::Arc, vec, vec::Vec};
use core::{cell::RefCell, mem::size_of};

use axerrno::{AxError, AxResult};
use axhal::time::monotonic_time_nanos;
use axtask::current;

use super::{
    defs::*,
    map::BpfMap,
    prog::{BpfMapBinding, BpfStreamState},
};
use crate::task::AsThread;

const MAP_TOKEN_BASE: u64 = 1;
const RING_TOKEN_BASE: u64 = 1 << 32;
/// Linux's `bpf_redirect_map()` helper number.  The shared UAPI crate owns
/// object layouts and commands; helper IDs remain local to the execution
/// policy because AXBPF intentionally has no Linux helper namespace.
const BPF_FUNC_REDIRECT_MAP: u32 = 51;
pub const XDP_CONTEXT_BYTES: usize = 24;
const XDP_ABORTED: u32 = 0;
const XDP_DROP: u32 = 1;
const XDP_PASS: u32 = 2;
const XDP_TX: u32 = 3;
const XDP_REDIRECT: u32 = 4;
/// The bounded packet capability advertised to the verifier for XDP `data`
/// and `data_end`.  Runtime capabilities carry the exact received-frame
/// length, so a verifier-approved load can never read beyond that frame.
pub const XDP_MAX_PACKET_BYTES: u32 = u16::MAX as u32;
const XDP_PACKET_REGION: axbpf::MemoryRegion = axbpf::MemoryRegion::Custom(1);
const RAW_TRACEPOINT_REGS_REGION: axbpf::MemoryRegion = axbpf::MemoryRegion::Custom(2);
const XDP_PACKET_TOKEN: u64 = 1;

/// The fixed x86_64 Linux `xdp_md` profile admitted by this kernel.  It is
/// serialized as native-endian u32 words because TheKernel is x86_64-only.
/// `data` and `data_end` are opaque packet offsets owned by the packet
/// pipeline, not user pointers and not capabilities writable by BPF.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XdpContext {
    pub data: u32,
    pub data_end: u32,
    pub data_meta: u32,
    pub ingress_ifindex: u32,
    pub rx_queue_index: u32,
    pub egress_ifindex: u32,
}

impl XdpContext {
    pub fn to_bytes(self) -> [u8; XDP_CONTEXT_BYTES] {
        let mut bytes = [0; XDP_CONTEXT_BYTES];
        for (slot, value) in bytes.chunks_exact_mut(size_of::<u32>()).zip([
            self.data,
            self.data_end,
            self.data_meta,
            self.ingress_ifindex,
            self.rx_queue_index,
            self.egress_ifindex,
        ]) {
            slot.copy_from_slice(&value.to_ne_bytes());
        }
        bytes
    }
}

/// An XSKMAP selection resolved at helper execution time.  The target is a
/// typed endpoint retained by the map slot; a numeric descriptor can neither
/// be observed nor recycled between helper execution and packet handoff.
#[derive(Clone)]
pub struct XdpRedirect {
    pub target: Arc<crate::file::af_xdp::XdpEndpoint>,
    pub flags: u32,
}

/// Terminal result of one XDP program invocation.  A `RedirectMiss` means a
/// program returned `XDP_REDIRECT` without a successfully selected XSKMAP
/// target, which the caller must treat as an aborted redirect rather than a
/// normal packet pass.
#[derive(Clone)]
pub enum XdpExecutionResult {
    Aborted,
    Drop,
    Pass,
    Tx,
    Redirect(XdpRedirect),
    RedirectMiss,
    Invalid(u32),
}

#[derive(Default)]
struct XdpRunState {
    redirect: Option<XdpRedirect>,
}
struct MapValue {
    map: Arc<dyn BpfMap>,
    key: Vec<u8>,
    data: Vec<u8>,
}
struct Reservation {
    map: Arc<dyn BpfMap>,
    data: Vec<u8>,
}
#[derive(Default)]
struct Resources {
    values: Vec<Option<MapValue>>,
    reservations: Vec<Option<Reservation>>,
    remaining: u64,
}

/// Per-run kernel resources, represented to AXBPF exclusively as capabilities.
pub struct BpfExecution<'a> {
    context: &'a mut [u8],
    resources: Rc<RefCell<Resources>>,
    maps: &'a [BpfMapBinding],
    /// A loaded program's standard-stream state, if this invocation has an
    /// owning program.  Keeping this as a borrowed sink prevents a helper
    /// call from extending the program lifetime or retaining an attachment.
    streams: Option<&'a [axsync::Mutex<BpfStreamState>; 2]>,
    /// Only XDP execution supplies this extra region.  It is immutable and
    /// represented to AXBPF as an opaque capability, never a host pointer.
    packet: Option<&'a [u8]>,
    /// A producer-owned pt_regs-style snapshot. It is borrowed only for the
    /// synchronous raw-tracepoint invocation and is never exposed as an
    /// unchecked host pointer to the interpreter.
    raw_tracepoint_regs: Option<&'a [u8]>,
}
impl<'a> BpfExecution<'a> {
    pub fn new(context: &'a mut [u8], maps: &'a [BpfMapBinding], budget: u64) -> Self {
        Self {
            context,
            maps,
            streams: None,
            packet: None,
            raw_tracepoint_regs: None,
            resources: Rc::new(RefCell::new(Resources {
                remaining: budget,
                ..Resources::default()
            })),
        }
    }

    pub fn with_raw_tracepoint_regs(mut self, regs: &'a [u8]) -> Self {
        self.raw_tracepoint_regs = Some(regs);
        self
    }

    /// Bind the invocation to the program's stdout/stderr streams.  Callers
    /// which execute a detached portable mechanism intentionally leave this
    /// unset, and stream-producing helpers then fail just as an unavailable
    /// kernel service would.
    pub fn with_streams(mut self, streams: &'a [axsync::Mutex<BpfStreamState>; 2]) -> Self {
        self.streams = Some(streams);
        self
    }
    fn execute_inner(
        &mut self,
        program: &axbpf::Program,
        xdp: Option<Rc<RefCell<XdpRunState>>>,
    ) -> AxResult<(u64, u64)> {
        let mut helpers = LinuxHelpers {
            maps: self.maps.to_vec(),
            resources: self.resources.clone(),
            streams: self.streams,
            context_len: self.context.len() as u32,
            xdp,
        };
        let result = program
            .execute(&mut helpers, self, axbpf::DEFAULT_MAX_EXECUTION_STEPS)
            .map_err(runtime_error)?;
        Ok((result, self.resources.borrow().remaining))
    }

    pub fn execute(mut self, program: &axbpf::Program) -> AxResult<(u64, u64)> {
        self.execute_inner(program, None)
    }

    /// Execute under the fixed read-only XDP context contract.  The result
    /// owns any XSKMAP target selected by `bpf_redirect_map`, so delivery is
    /// an explicit later packet-pipeline operation rather than a side effect
    /// performed by an untyped helper.
    pub fn execute_xdp(
        mut self,
        program: &axbpf::Program,
        packet: &'a [u8],
    ) -> AxResult<(XdpExecutionResult, u64)> {
        if self.context.len() != XDP_CONTEXT_BYTES || packet.len() > XDP_MAX_PACKET_BYTES as usize {
            return Err(AxError::InvalidInput);
        }
        self.packet = Some(packet);
        let xdp = Rc::new(RefCell::new(XdpRunState::default()));
        let (returned, remaining) = self.execute_inner(program, Some(xdp.clone()))?;
        let terminal = match returned as u32 {
            XDP_ABORTED => XdpExecutionResult::Aborted,
            XDP_DROP => XdpExecutionResult::Drop,
            XDP_PASS => XdpExecutionResult::Pass,
            XDP_TX => XdpExecutionResult::Tx,
            XDP_REDIRECT => xdp
                .borrow_mut()
                .redirect
                .take()
                .map(XdpExecutionResult::Redirect)
                .unwrap_or(XdpExecutionResult::RedirectMiss),
            value => XdpExecutionResult::Invalid(value),
        };
        Ok((terminal, remaining))
    }
    fn index(cap: axbpf::Capability, base: u64, count: usize) -> Option<usize> {
        cap.token
            .checked_sub(base)
            .and_then(|x| usize::try_from(x).ok())
            .filter(|&x| x < count)
    }
}
impl axbpf::ExecutionContext for BpfExecution<'_> {
    fn read(&mut self, cap: axbpf::Capability, offset: usize, out: &mut [u8]) -> bool {
        let Some(end) = offset.checked_add(out.len()) else {
            return false;
        };
        match cap.region {
            axbpf::MemoryRegion::Context if cap.token == 0 && end <= self.context.len() => {
                out.copy_from_slice(&self.context[offset..end]);
                true
            }
            XDP_PACKET_REGION if cap.token == XDP_PACKET_TOKEN => {
                let Some(packet) = self.packet else {
                    return false;
                };
                if end > packet.len() {
                    return false;
                }
                out.copy_from_slice(&packet[offset..end]);
                true
            }
            RAW_TRACEPOINT_REGS_REGION if cap.token == 0 => {
                let Some(regs) = self.raw_tracepoint_regs else {
                    return false;
                };
                if end > regs.len() {
                    return false;
                }
                out.copy_from_slice(&regs[offset..end]);
                true
            }
            axbpf::MemoryRegion::MapValue => {
                let r = self.resources.borrow();
                let Some(i) = Self::index(cap, MAP_TOKEN_BASE, r.values.len()) else {
                    return false;
                };
                let Some(value) = r.values[i].as_ref() else {
                    return false;
                };
                if end > value.data.len() {
                    return false;
                }
                out.copy_from_slice(&value.data[offset..end]);
                true
            }
            axbpf::MemoryRegion::RingReservation => {
                let r = self.resources.borrow();
                let Some(i) = Self::index(cap, RING_TOKEN_BASE, r.reservations.len()) else {
                    return false;
                };
                let Some(reservation) = r.reservations[i].as_ref() else {
                    return false;
                };
                if end > reservation.data.len() {
                    return false;
                }
                out.copy_from_slice(&reservation.data[offset..end]);
                true
            }
            _ => false,
        }
    }
    fn write(&mut self, cap: axbpf::Capability, offset: usize, input: &[u8]) -> bool {
        if !cap.writable {
            return false;
        }
        let Some(end) = offset.checked_add(input.len()) else {
            return false;
        };
        match cap.region {
            axbpf::MemoryRegion::Context if cap.token == 0 && end <= self.context.len() => {
                self.context[offset..end].copy_from_slice(input);
                true
            }
            axbpf::MemoryRegion::MapValue => {
                let mut r = self.resources.borrow_mut();
                let Some(i) = Self::index(cap, MAP_TOKEN_BASE, r.values.len()) else {
                    return false;
                };
                let (key_len, data_len) = {
                    let Some(v) = r.values[i].as_ref() else {
                        return false;
                    };
                    (v.key.len(), v.data.len())
                };
                let cost = (key_len + data_len) as u64;
                if end > data_len || r.remaining < cost {
                    return false;
                }
                r.remaining -= cost;
                let Some(v) = r.values[i].as_mut() else {
                    return false;
                };
                v.data[offset..end].copy_from_slice(input);
                v.map.update(&v.key, &v.data, BPF_ANY).is_ok()
            }
            axbpf::MemoryRegion::RingReservation => {
                let mut r = self.resources.borrow_mut();
                let Some(i) = Self::index(cap, RING_TOKEN_BASE, r.reservations.len()) else {
                    return false;
                };
                let Some(reservation) = r.reservations[i].as_mut() else {
                    return false;
                };
                if end > reservation.data.len() {
                    return false;
                }
                reservation.data[offset..end].copy_from_slice(input);
                true
            }
            _ => false,
        }
    }

    fn context_pointer(
        &mut self,
        cap: axbpf::Capability,
        offset: usize,
        width: usize,
    ) -> Option<axbpf::Capability> {
        // xdp_md.data and xdp_md.data_end are pointer-typed in Linux.  Both
        // name the same bounded packet object here; `data_end` remains an
        // independently typed capability for the verifier's normal bounds
        // comparisons, while every actual dereference is checked against the
        // exact received frame length below.
        (cap.region == axbpf::MemoryRegion::Context
            && cap.token == 0
            && width == size_of::<u32>()
            && matches!(offset, 0 | 4)
            && self.packet.is_some())
        .then(|| {
            let length = self.packet.unwrap().len() as u32;
            axbpf::Capability {
                region: XDP_PACKET_REGION,
                token: XDP_PACKET_TOKEN,
                // `data_end` is a one-past-the-end capability.  The VM
                // permits comparing it to a `data + constant` capability
                // from this same packet, but any dereference at this
                // offset remains rejected by the normal bounds check.
                offset: if offset == 4 { length as i32 } else { 0 },
                length,
                writable: false,
            }
        })
        .or_else(|| {
            (cap.region == axbpf::MemoryRegion::Context
                && cap.token == 0
                && offset == 0
                && width == size_of::<u64>()
                && self.raw_tracepoint_regs.is_some())
            .then(|| axbpf::Capability {
                region: RAW_TRACEPOINT_REGS_REGION,
                token: 0,
                offset: 0,
                length: self.raw_tracepoint_regs.unwrap().len() as u32,
                writable: false,
            })
        })
    }
}

pub struct LinuxHelperPolicy {
    pub allow_perf_event: bool,
    pub allow_xdp_redirect: bool,
}
impl axbpf::HelperSet for LinuxHelperPolicy {
    fn signature(&self, id: u32) -> Option<axbpf::HelperSignature> {
        use axbpf::{ArgKind as A, HelperSignature as S, ReturnKind as R};
        let ptr = |readable, writable| A::Pointer { readable, writable };
        let scalar = || S {
            args: [None; 5],
            result: R::Scalar,
        };
        Some(match id {
            BPF_FUNC_MAP_LOOKUP_ELEM => S {
                args: [Some(A::Map), Some(ptr(true, false)), None, None, None],
                result: R::MapValueOrNull,
            },
            BPF_FUNC_MAP_UPDATE_ELEM => S {
                args: [
                    Some(A::Map),
                    Some(ptr(true, false)),
                    Some(ptr(true, false)),
                    Some(A::Scalar),
                    None,
                ],
                result: R::Scalar,
            },
            BPF_FUNC_MAP_DELETE_ELEM => S {
                args: [Some(A::Map), Some(ptr(true, false)), None, None, None],
                result: R::Scalar,
            },
            BPF_FUNC_GET_CURRENT_COMM => S {
                args: [Some(ptr(false, true)), Some(A::Scalar), None, None, None],
                result: R::Scalar,
            },
            BPF_FUNC_RINGBUF_OUTPUT => S {
                args: [
                    Some(A::Map),
                    Some(ptr(true, false)),
                    Some(A::Scalar),
                    Some(A::Scalar),
                    None,
                ],
                result: R::Scalar,
            },
            BPF_FUNC_RINGBUF_RESERVE => S {
                args: [Some(A::Map), Some(A::Scalar), Some(A::Scalar), None, None],
                result: R::NullablePointer {
                    region: axbpf::MemoryRegion::RingReservation,
                    length: u32::MAX,
                    writable: true,
                },
            },
            BPF_FUNC_RINGBUF_SUBMIT | BPF_FUNC_RINGBUF_DISCARD => S {
                args: [Some(ptr(false, true)), Some(A::Scalar), None, None, None],
                result: R::Scalar,
            },
            BPF_FUNC_PERF_EVENT_READ if self.allow_perf_event => S {
                args: [Some(A::Map), Some(A::Scalar), None, None, None],
                result: R::Scalar,
            },
            BPF_FUNC_PERF_EVENT_OUTPUT if self.allow_perf_event => S {
                args: [
                    Some(ptr(true, false)),
                    Some(A::Map),
                    Some(A::Scalar),
                    Some(ptr(true, false)),
                    Some(A::Scalar),
                ],
                result: R::Scalar,
            },
            BPF_FUNC_PERF_EVENT_READ_VALUE if self.allow_perf_event => S {
                args: [
                    Some(A::Map),
                    Some(A::Scalar),
                    Some(ptr(false, true)),
                    Some(A::Scalar),
                    None,
                ],
                result: R::Scalar,
            },
            BPF_FUNC_TRACE_PRINTK => S {
                args: [
                    Some(ptr(true, false)),
                    Some(A::Scalar),
                    Some(A::Scalar),
                    Some(A::Scalar),
                    Some(A::Scalar),
                ],
                result: R::Scalar,
            },
            BPF_FUNC_TAIL_CALL => S {
                args: [
                    Some(ptr(true, false)),
                    Some(A::Map),
                    Some(A::Scalar),
                    None,
                    None,
                ],
                result: R::Scalar,
            },
            BPF_FUNC_REDIRECT_MAP if self.allow_xdp_redirect => S {
                args: [Some(A::Map), Some(A::Scalar), Some(A::Scalar), None, None],
                result: R::Scalar,
            },
            BPF_FUNC_KTIME_GET_NS
            | BPF_FUNC_GET_CURRENT_PID_TGID
            | BPF_FUNC_GET_CURRENT_UID_GID
            | BPF_FUNC_GET_PRANDOM_U32
            | BPF_FUNC_GET_SMP_PROCESSOR_ID => scalar(),
            _ => return None,
        })
    }
    fn call(
        &mut self,
        _: u32,
        _: [axbpf::Value; 5],
        _: &mut dyn axbpf::HelperMemory,
    ) -> Result<axbpf::Value, axbpf::RuntimeError> {
        Err(axbpf::RuntimeError::Helper)
    }
}
struct LinuxHelpers<'a> {
    maps: Vec<BpfMapBinding>,
    resources: Rc<RefCell<Resources>>,
    streams: Option<&'a [axsync::Mutex<BpfStreamState>; 2]>,
    context_len: u32,
    xdp: Option<Rc<RefCell<XdpRunState>>>,
}
impl LinuxHelpers<'_> {
    fn map(&self, v: axbpf::Value) -> Option<Arc<dyn BpfMap>> {
        let axbpf::Value::Map(reference) = v else {
            return None;
        };
        self.maps
            .iter()
            .find(|x| x.reference == reference)
            .map(|x| x.map.clone())
    }
    fn scalar(v: axbpf::Value) -> Option<u64> {
        v.scalar()
    }
    fn read(m: &mut dyn axbpf::HelperMemory, v: axbpf::Value, n: usize) -> Option<Vec<u8>> {
        let axbpf::Value::Pointer(c) = v else {
            return None;
        };
        let mut b = vec![0; n];
        m.read(c, c.offset as usize, &mut b).then_some(b)
    }
    fn error() -> axbpf::Value {
        axbpf::Value::Scalar(u64::MAX)
    }

    fn push_standard_stream(&self, stream_id: u32, bytes: &[u8]) -> AxResult<()> {
        let index = match stream_id {
            1 => 0,
            2 => 1,
            _ => return Err(AxError::NotFound),
        };
        let streams = self.streams.ok_or(AxError::NotFound)?;
        streams[index].lock().push(bytes)
    }
    fn call_inner(
        &mut self,
        id: u32,
        a: [axbpf::Value; 5],
        m: &mut dyn axbpf::HelperMemory,
    ) -> AxResult<axbpf::Value> {
        match id {
            BPF_FUNC_MAP_LOOKUP_ELEM => {
                let Some(map) = self.map(a[0]) else {
                    return Ok(axbpf::Value::Scalar(0));
                };
                let Some(key) = Self::read(m, a[1], map.key_size() as usize) else {
                    return Ok(axbpf::Value::Scalar(0));
                };
                let Some(data) = map.lookup(&key) else {
                    return Ok(axbpf::Value::Scalar(0));
                };
                let mut r = self.resources.borrow_mut();
                if r.remaining < data.len() as u64 {
                    return Err(AxError::ResourceBusy);
                }
                r.remaining -= data.len() as u64;
                let token = MAP_TOKEN_BASE + r.values.len() as u64;
                let length = data.len() as u32;
                r.values.push(Some(MapValue { map, key, data }));
                Ok(axbpf::Value::Pointer(axbpf::Capability {
                    region: axbpf::MemoryRegion::MapValue,
                    token,
                    offset: 0,
                    length,
                    writable: true,
                }))
            }
            BPF_FUNC_MAP_UPDATE_ELEM => {
                let Some(map) = self.map(a[0]) else {
                    return Ok(Self::error());
                };
                let (Some(key), Some(data), Some(flags)) = (
                    Self::read(m, a[1], map.key_size() as usize),
                    Self::read(m, a[2], map.value_size() as usize),
                    Self::scalar(a[3]),
                ) else {
                    return Ok(Self::error());
                };
                let mut r = self.resources.borrow_mut();
                let cost = (key.len() + data.len()) as u64;
                if r.remaining < cost {
                    return Err(AxError::ResourceBusy);
                }
                r.remaining -= cost;
                if map.update(&key, &data, flags).is_ok() {
                    r.values.iter_mut().for_each(|value| *value = None);
                    Ok(axbpf::Value::Scalar(0))
                } else {
                    Ok(Self::error())
                }
            }
            BPF_FUNC_MAP_DELETE_ELEM => {
                let Some(map) = self.map(a[0]) else {
                    return Ok(Self::error());
                };
                let Some(key) = Self::read(m, a[1], map.key_size() as usize) else {
                    return Ok(Self::error());
                };
                let mut r = self.resources.borrow_mut();
                if map.delete(&key).is_ok() {
                    r.values.iter_mut().for_each(|value| *value = None);
                    Ok(axbpf::Value::Scalar(0))
                } else {
                    Ok(Self::error())
                }
            }
            BPF_FUNC_RINGBUF_OUTPUT => {
                let Some(map) = self.map(a[0]) else {
                    return Ok(Self::error());
                };
                let (Some(size), Some(flags)) = (Self::scalar(a[2]), Self::scalar(a[3])) else {
                    return Ok(Self::error());
                };
                let Some(data) = Self::read(m, a[1], size as usize) else {
                    return Ok(Self::error());
                };
                let mut r = self.resources.borrow_mut();
                if r.remaining < data.len() as u64 {
                    return Err(AxError::ResourceBusy);
                }
                r.remaining -= data.len() as u64;
                if map.ringbuf_output(&data, flags).is_ok() {
                    Ok(axbpf::Value::Scalar(0))
                } else {
                    Ok(Self::error())
                }
            }
            BPF_FUNC_RINGBUF_RESERVE => {
                let Some(map) = self.map(a[0]) else {
                    return Ok(axbpf::Value::Scalar(0));
                };
                let (Some(size), Some(flags)) = (Self::scalar(a[1]), Self::scalar(a[2])) else {
                    return Ok(axbpf::Value::Scalar(0));
                };
                let size = size as usize;
                let mut r = self.resources.borrow_mut();
                if r.remaining < size as u64 || map.ringbuf_reserve(size, flags).is_err() {
                    return Ok(axbpf::Value::Scalar(0));
                }
                r.remaining -= size as u64;
                let token = RING_TOKEN_BASE + r.reservations.len() as u64;
                r.reservations.push(Some(Reservation {
                    map,
                    data: vec![0; size],
                }));
                Ok(axbpf::Value::Pointer(axbpf::Capability {
                    region: axbpf::MemoryRegion::RingReservation,
                    token,
                    offset: 0,
                    length: size as u32,
                    writable: true,
                }))
            }
            BPF_FUNC_RINGBUF_SUBMIT | BPF_FUNC_RINGBUF_DISCARD => {
                let axbpf::Value::Pointer(c) = a[0] else {
                    return Ok(Self::error());
                };
                if c.region != axbpf::MemoryRegion::RingReservation || c.offset != 0 {
                    return Ok(Self::error());
                };
                let mut r = self.resources.borrow_mut();
                let Some(i) = BpfExecution::index(c, RING_TOKEN_BASE, r.reservations.len()) else {
                    return Ok(Self::error());
                };
                let Some(reservation) = r.reservations[i].take() else {
                    return Ok(Self::error());
                };
                let flags = Self::scalar(a[1]).unwrap_or(0);
                let out = if id == BPF_FUNC_RINGBUF_SUBMIT {
                    reservation.map.ringbuf_submit(reservation.data, flags)
                } else {
                    reservation
                        .map
                        .ringbuf_discard(reservation.data.len(), flags)
                };
                if out.is_ok() {
                    Ok(axbpf::Value::Scalar(0))
                } else {
                    Ok(Self::error())
                }
            }
            BPF_FUNC_PERF_EVENT_READ => {
                let (Some(map), Some(flags)) = (self.map(a[0]), Self::scalar(a[1])) else {
                    return Ok(Self::error());
                };
                let index = if flags == BPF_F_CURRENT_CPU {
                    axhal::percpu::this_cpu_id() as u32
                } else if flags & !BPF_F_INDEX_MASK == 0 {
                    flags as u32
                } else {
                    return Ok(Self::error());
                };
                match map.perf_event_read_value(index) {
                    Ok((counter, ..)) => Ok(axbpf::Value::Scalar(counter)),
                    Err(_) => Ok(Self::error()),
                }
            }
            BPF_FUNC_PERF_EVENT_OUTPUT => {
                let (Some(map), Some(flags), Some(size)) =
                    (self.map(a[1]), Self::scalar(a[2]), Self::scalar(a[4]))
                else {
                    return Ok(Self::error());
                };
                let Ok(size) = usize::try_from(size) else {
                    return Ok(Self::error());
                };
                // This bound is both the ABI promise for the currently
                // implemented raw payload and the maximum temporary source
                // buffer needed by the portable VM adapter.
                if size > 4096 {
                    return Ok(Self::error());
                }
                let index = if flags == BPF_F_CURRENT_CPU {
                    axhal::percpu::this_cpu_id() as u32
                } else if flags & !BPF_F_INDEX_MASK == 0 {
                    flags as u32
                } else {
                    return Ok(Self::error());
                };
                let Some(data) = Self::read(m, a[3], size) else {
                    return Ok(Self::error());
                };
                if map.perf_event_output(index, &data).is_ok() {
                    Ok(axbpf::Value::Scalar(0))
                } else {
                    Ok(Self::error())
                }
            }
            BPF_FUNC_PERF_EVENT_READ_VALUE => {
                let (Some(map), Some(flags), Some(size)) =
                    (self.map(a[0]), Self::scalar(a[1]), Self::scalar(a[3]))
                else {
                    return Ok(Self::error());
                };
                if size < core::mem::size_of::<BpfPerfEventValue>() as u64 {
                    return Ok(Self::error());
                }
                let index = if flags == BPF_F_CURRENT_CPU {
                    axhal::percpu::this_cpu_id() as u32
                } else if flags & !BPF_F_INDEX_MASK == 0 {
                    flags as u32
                } else {
                    return Ok(Self::error());
                };
                let Ok((counter, enabled, running)) = map.perf_event_read_value(index) else {
                    return Ok(Self::error());
                };
                let value = BpfPerfEventValue {
                    counter,
                    enabled,
                    running,
                };
                let axbpf::Value::Pointer(capability) = a[2] else {
                    return Ok(Self::error());
                };
                if m.write(
                    capability,
                    capability.offset as usize,
                    bytemuck::bytes_of(&value),
                ) {
                    Ok(axbpf::Value::Scalar(0))
                } else {
                    Ok(Self::error())
                }
            }
            BPF_FUNC_KTIME_GET_NS => Ok(axbpf::Value::Scalar(monotonic_time_nanos())),
            BPF_FUNC_GET_CURRENT_UID_GID => Ok(axbpf::Value::Scalar(0)),
            BPF_FUNC_GET_SMP_PROCESSOR_ID => {
                Ok(axbpf::Value::Scalar(axhal::percpu::this_cpu_id() as u64))
            }
            BPF_FUNC_GET_CURRENT_PID_TGID => {
                let x = current()
                    .try_as_thread()
                    .map(|t| ((t.proc_data.proc.pid() as u64) << 32) | t.tid() as u64)
                    .unwrap_or(0);
                Ok(axbpf::Value::Scalar(x))
            }
            BPF_FUNC_GET_PRANDOM_U32 => {
                let mut x = monotonic_time_nanos() as u32;
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                Ok(axbpf::Value::Scalar(x as u64))
            }
            BPF_FUNC_GET_CURRENT_COMM => {
                let (axbpf::Value::Pointer(c), Some(size)) = (a[0], Self::scalar(a[1])) else {
                    return Ok(Self::error());
                };
                let name = current().try_name().map_err(|_| AxError::ResourceBusy)?;
                let size = (size as usize).min(16);
                let mut out = vec![0; size];
                let n = name.len().min(size.saturating_sub(1));
                out[..n].copy_from_slice(&name.as_bytes()[..n]);
                Ok(if m.write(c, c.offset as usize, &out) {
                    axbpf::Value::Scalar(0)
                } else {
                    Self::error()
                })
            }
            BPF_FUNC_TRACE_PRINTK => {
                let n = Self::scalar(a[1]).unwrap_or(0).min(256) as usize;
                let Some(bytes) = (n != 0).then(|| Self::read(m, a[0], n)).flatten() else {
                    return Ok(Self::error());
                };
                // Standard BPF program streams are the kernel-visible
                // producer behind PROG_STREAM_READ_BY_FD.  AXBPF currently
                // represents the verifier's staged printk producer as this
                // helper call; publish the fully copied record atomically.
                Ok(if self.push_standard_stream(1, &bytes).is_ok() {
                    axbpf::Value::Scalar(bytes.len() as u64)
                } else {
                    Self::error()
                })
            }
            BPF_FUNC_REDIRECT_MAP => {
                let (Some(xdp), Some(map), Some(index), Some(flags)) = (
                    self.xdp.as_ref(),
                    self.map(a[0]),
                    Self::scalar(a[1]).and_then(|value| u32::try_from(value).ok()),
                    Self::scalar(a[2]).and_then(|value| u32::try_from(value).ok()),
                ) else {
                    return Ok(Self::error());
                };
                // This foundation deliberately accepts only XSKMAP.  DEV/CPU
                // map forwarding needs a device/CPU ownership contract and is
                // not silently approximated as an AF_XDP handoff.
                if map.map_type() != BPF_MAP_TYPE_XSKMAP {
                    return Ok(Self::error());
                }
                // The low two bits are Linux's requested action if terminal
                // redirect resolution fails.  XSKMAP has no broadcast or
                // device-only redirect extensions in this implementation.
                if flags & !0x3 != 0 {
                    return Ok(Self::error());
                }
                let Some(target) = map.xsk_redirect_target(index) else {
                    return Ok(axbpf::Value::Scalar((flags & 0x3) as u64));
                };
                xdp.borrow_mut().redirect = Some(XdpRedirect { target, flags });
                Ok(axbpf::Value::Scalar(XDP_REDIRECT as u64))
            }
            // A miss, invalid map type, or exhausted tail depth is a normal
            // tail-call failure: execution continues in the caller with 0.
            BPF_FUNC_TAIL_CALL => Ok(axbpf::Value::Scalar(0)),
            _ => Ok(Self::error()),
        }
    }
}
impl axbpf::HelperSet for LinuxHelpers<'_> {
    fn signature(&self, id: u32) -> Option<axbpf::HelperSignature> {
        <LinuxHelperPolicy as axbpf::HelperSet>::signature(
            &LinuxHelperPolicy {
                allow_perf_event: false,
                allow_xdp_redirect: self.xdp.is_some(),
            },
            id,
        )
    }
    fn call(
        &mut self,
        id: u32,
        a: [axbpf::Value; 5],
        m: &mut dyn axbpf::HelperMemory,
    ) -> Result<axbpf::Value, axbpf::RuntimeError> {
        self.call_inner(id, a, m)
            .map_err(|_| axbpf::RuntimeError::Helper)
    }
    fn tail_call(
        &mut self,
        args: [axbpf::Value; 5],
    ) -> Result<Option<axbpf::Program>, axbpf::RuntimeError> {
        let Some(map) = self.map(args[1]) else {
            return Ok(None);
        };
        let Some(index) = Self::scalar(args[2]).and_then(|index| u32::try_from(index).ok()) else {
            return Ok(None);
        };
        let Some(program) = map.tail_call_program(index) else {
            return Ok(None);
        };
        if program.mechanism.required_context_bytes() > self.context_len {
            return Ok(None);
        }
        self.maps = program.maps.clone();
        Ok(Some(program.mechanism.clone()))
    }
}
fn runtime_error(e: axbpf::RuntimeError) -> AxError {
    match e {
        axbpf::RuntimeError::StepLimit => AxError::ResourceBusy,
        axbpf::RuntimeError::Memory | axbpf::RuntimeError::Bounds => AxError::BadAddress,
        _ => AxError::InvalidInput,
    }
}
