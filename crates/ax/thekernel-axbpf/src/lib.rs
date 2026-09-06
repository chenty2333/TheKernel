#![no_std]
#![forbid(unsafe_code)]
//! Policy-neutral eBPF decoding, verification, interpretation, and map primitives.
//!
//! This crate deliberately has no object handles, syscall ABI, error-number, task,
//! or user-memory concepts.  Embedders supply maps, helpers, and execution memory.

extern crate alloc;

use alloc::{collections::VecDeque, vec, vec::Vec};

pub const REGISTER_COUNT: usize = 11;
pub const FRAME_POINTER: usize = 10;
pub const DEFAULT_MAX_INSTRUCTIONS: usize = 4096;
pub const DEFAULT_STACK_BYTES: usize = 512;
/// Default bounded number of instructions executed by one runtime invocation.
pub const DEFAULT_MAX_EXECUTION_STEPS: usize = 1_000_000;
/// eBPF atomic memory-operation mode (used with `STX`).
pub const ATOMIC: u8 = 0xc0;
/// eBPF atomic-operation flag requesting the pre-operation value in the destination.
pub const FETCH: i32 = 1;

/// The portable eight-byte eBPF instruction encoding.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, bytemuck::AnyBitPattern, bytemuck::NoUninit)]
pub struct Instruction {
    pub code: u8,
    pub regs: u8,
    pub off: i16,
    pub imm: i32,
}
const _: [(); 8] = [(); core::mem::size_of::<Instruction>()];
impl Instruction {
    pub const fn dst(self) -> u8 {
        self.regs & 15
    }
    pub const fn src(self) -> u8 {
        self.regs >> 4
    }
    pub const fn class(self) -> u8 {
        self.code & 7
    }
    pub const fn op(self) -> u8 {
        self.code & 0xf0
    }
}

/// ISA encoding constants; these describe bytecode, not an operating-system ABI.
pub mod opcode {
    pub const LD: u8 = 0;
    pub const LDX: u8 = 1;
    pub const ST: u8 = 2;
    pub const STX: u8 = 3;
    pub const ALU: u8 = 4;
    pub const JMP: u8 = 5;
    pub const JMP32: u8 = 6;
    pub const ALU64: u8 = 7;
    pub const K: u8 = 0;
    pub const X: u8 = 8;
    pub const W: u8 = 0;
    pub const H: u8 = 8;
    pub const B: u8 = 16;
    pub const DW: u8 = 24;
    pub const MEM: u8 = 96;
    pub const IMM: u8 = 0;
    pub const ADD: u8 = 0;
    pub const SUB: u8 = 16;
    pub const MUL: u8 = 32;
    pub const DIV: u8 = 48;
    pub const OR: u8 = 64;
    pub const AND: u8 = 80;
    pub const LSH: u8 = 96;
    pub const RSH: u8 = 112;
    pub const NEG: u8 = 128;
    pub const MOD: u8 = 144;
    pub const XOR: u8 = 160;
    pub const MOV: u8 = 176;
    pub const ARSH: u8 = 192;
    pub const END: u8 = 208;
    pub const JA: u8 = 0;
    pub const JEQ: u8 = 16;
    pub const JGT: u8 = 32;
    pub const JGE: u8 = 48;
    pub const JSET: u8 = 64;
    pub const JNE: u8 = 80;
    pub const JSGT: u8 = 96;
    pub const JSGE: u8 = 112;
    pub const CALL: u8 = 128;
    pub const EXIT: u8 = 144;
    pub const JLT: u8 = 160;
    pub const JLE: u8 = 176;
    pub const JSLT: u8 = 192;
    pub const JSLE: u8 = 208;
    pub const PSEUDO_MAP: u8 = 1;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapRef(pub u32);
impl MapRef {
    /// Creates a map reference from the signed file-descriptor form used by
    /// `BPF_PSEUDO_MAP_FD` wide immediates.
    pub const fn from_fd(fd: i32) -> Self {
        Self(fd as u32)
    }
    /// Returns the signed file-descriptor form carried by a pseudo-map load.
    pub const fn fd(self) -> i32 {
        self.0 as i32
    }
}
/// Resolves an embedding-defined map reference.  The verifier never owns a map.
pub trait MapResolver {
    fn resolve(&self, reference: MapRef) -> Option<MapInfo>;
    /// Resolves the file-descriptor payload of a `BPF_PSEUDO_MAP_FD` load.
    ///
    /// The default preserves the original `MapRef` API while allowing fd-based
    /// kernel map tables to override just this method.
    fn resolve_fd(&self, fd: i32) -> Option<MapInfo> {
        self.resolve(MapRef::from_fd(fd))
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapInfo {
    pub key_size: u32,
    pub value_size: u32,
    pub max_entries: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryRegion {
    Context,
    Stack,
    MapValue,
    RingReservation,
    Custom(u16),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capability {
    pub region: MemoryRegion,
    /// Embedding-defined identity of the backing object.  This distinguishes
    /// concurrent map values and ring reservations in the same region.
    pub token: u64,
    pub offset: i32,
    pub length: u32,
    pub writable: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Value {
    Scalar(u64),
    Pointer(Capability),
    Map(MapRef),
    Uninit,
}
impl Value {
    pub const fn scalar(self) -> Option<u64> {
        if let Self::Scalar(x) = self {
            Some(x)
        } else {
            None
        }
    }
}

/// Domain-owned memory backing capability accesses.  A false return is a denied access.
pub trait ExecutionContext {
    fn read(&mut self, capability: Capability, offset: usize, out: &mut [u8]) -> bool;
    fn write(&mut self, capability: Capability, offset: usize, input: &[u8]) -> bool;
    /// Resolve a context-field load to an embedding-owned capability.  This
    /// is deliberately opt-in through [`VerifyPolicy::context_pointer_fields`]
    /// so ordinary context bytes remain scalars.  It lets a typed producer
    /// expose an opaque pointer (such as XDP's `data`/`data_end`) without ever
    /// materialising a host address in a BPF register.
    fn context_pointer(
        &mut self,
        _capability: Capability,
        _offset: usize,
        _width: usize,
    ) -> Option<Capability> {
        None
    }
    /// Performs one atomic read-modify-write operation and returns the old value.
    /// Implementations that expose shared map/ring memory should override this;
    /// the default rejects atomics rather than silently making them non-atomic.
    fn atomic(
        &mut self,
        _capability: Capability,
        _offset: usize,
        _width: u32,
        _operation: AtomicOperation,
    ) -> Result<u64, RuntimeError> {
        Err(RuntimeError::Memory)
    }
}
/// Memory made available to a helper while a program is running.  Unlike the
/// embedding context this also exposes the VM-owned stack, but only through a
/// capability supplied by the program.
pub trait HelperMemory {
    fn read(&mut self, capability: Capability, offset: usize, out: &mut [u8]) -> bool;
    fn write(&mut self, capability: Capability, offset: usize, input: &[u8]) -> bool;
    fn atomic(
        &mut self,
        capability: Capability,
        offset: usize,
        width: u32,
        operation: AtomicOperation,
    ) -> Result<u64, RuntimeError>;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtomicOperation {
    Add(u64),
    Or(u64),
    And(u64),
    Xor(u64),
    Exchange(u64),
    CompareExchange { expected: u64, new: u64 },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArgKind {
    Scalar,
    Pointer { readable: bool, writable: bool },
    Map,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReturnKind {
    Scalar,
    Pointer {
        region: MemoryRegion,
        length: u32,
        writable: bool,
    },
    /// A helper result that is either a capability pointer or the scalar null
    /// value.  Test it against zero before using it as a pointer.
    NullablePointer {
        region: MemoryRegion,
        length: u32,
        writable: bool,
    },
    MapValueOrNull,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelperSignature {
    pub args: [Option<ArgKind>; 5],
    pub result: ReturnKind,
}
/// Abstract arguments supplied to a helper's verify-time effect calculation.
/// This lets an embedding derive a map-value result size from the particular
/// map argument without granting the verifier access to the map object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelperArgument {
    Uninit,
    Scalar,
    Pointer {
        region: MemoryRegion,
        length: u32,
        writable: bool,
    },
    Map(MapRef),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelperEffects {
    pub result: ReturnKind,
}
/// Helper identifiers are embedding-defined; this is both the verify-time and run-time authority.
pub trait HelperSet {
    fn signature(&self, id: u32) -> Option<HelperSignature>;
    /// Returns the result effect for these verified argument types.  The
    /// default is the signature's fixed result; map-aware helpers can override
    /// this to return a capability sized for the supplied map.
    fn effects(&self, id: u32, _args: [HelperArgument; 5]) -> Option<HelperEffects> {
        self.signature(id).map(|signature| HelperEffects {
            result: signature.result,
        })
    }
    fn call(
        &mut self,
        id: u32,
        args: [Value; 5],
        memory: &mut dyn HelperMemory,
    ) -> Result<Value, RuntimeError>;
    /// A successful tail call replaces the current program without returning
    /// to the caller.  The default keeps existing embeddings unchanged.
    fn tail_call(&mut self, _args: [Value; 5]) -> Result<Option<Program>, RuntimeError> {
        Ok(None)
    }
}
pub trait HelperResolver: HelperSet {}
impl<T: HelperSet + ?Sized> HelperResolver for T {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifyPolicy {
    pub max_instructions: usize,
    pub stack_bytes: usize,
    pub context_bytes: u32,
    /// Whether the embedding context is writable by program stores/helpers.
    pub context_writable: bool,
    /// Exact scalar context fields which instead produce a typed capability
    /// when loaded at its declared width. The fixed-size array keeps this no-std policy
    /// copyable and avoids making every ordinary embedding allocate metadata.
    pub context_pointer_fields: [Option<ContextPointerField>; 3],
    pub allow_loops: bool,
    pub max_states: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextPointerField {
    pub offset: u32,
    /// Width of the scalar load which materializes this capability (4 or 8).
    pub width: u8,
    pub region: MemoryRegion,
    pub max_length: u32,
    pub writable: bool,
}
impl Default for VerifyPolicy {
    fn default() -> Self {
        Self {
            max_instructions: DEFAULT_MAX_INSTRUCTIONS,
            stack_bytes: DEFAULT_STACK_BYTES,
            context_bytes: 0,
            context_writable: false,
            context_pointer_fields: [None; 3],
            allow_loops: false,
            max_states: DEFAULT_MAX_INSTRUCTIONS * 4,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerifyError {
    Empty,
    TooLong,
    BadRegister(usize),
    BadEncoding(usize),
    BadOpcode(usize),
    BadJump(usize),
    Loop(usize),
    Unreachable(usize),
    MissingExit,
    UnknownMap(usize),
    UnknownHelper(usize),
    Type(usize),
    Bounds(usize),
    StateLimit,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeError {
    StepLimit,
    InvalidProgramCounter,
    Type,
    Bounds,
    Memory,
    Helper,
    Arithmetic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decoded {
    Basic,
    WideImmediate(u64),
    Map(MapRef),
    Continuation { head: usize },
}
pub fn decode(
    instructions: &[Instruction],
    maps: &impl MapResolver,
) -> Result<Vec<Decoded>, VerifyError> {
    let mut result = vec![Decoded::Basic; instructions.len()];
    let mut i = 0;
    while i < instructions.len() {
        let x = instructions[i];
        if x.dst() > 10 || x.src() > 10 {
            return Err(VerifyError::BadRegister(i));
        }
        if x.class() == opcode::LD
            && (x.code & 0xe0) == opcode::IMM
            && (x.code & 0x18) == opcode::DW
        {
            let Some(next) = instructions.get(i + 1) else {
                return Err(VerifyError::BadEncoding(i));
            };
            if next.code != 0 || next.regs != 0 || next.off != 0 {
                return Err(VerifyError::BadEncoding(i + 1));
            }
            result[i] = if x.src() == 0 {
                Decoded::WideImmediate((x.imm as u32 as u64) | ((next.imm as u32 as u64) << 32))
            } else if x.src() == opcode::PSEUDO_MAP {
                let r = MapRef::from_fd(x.imm);
                if maps.resolve_fd(x.imm).is_none() {
                    return Err(VerifyError::UnknownMap(i));
                };
                Decoded::Map(r)
            } else {
                return Err(VerifyError::BadEncoding(i));
            };
            result[i + 1] = Decoded::Continuation { head: i };
            i += 2;
        } else {
            i += 1;
        }
    }
    Ok(result)
}

#[derive(Clone, Debug)]
pub struct Program {
    instructions: Vec<Instruction>,
    decoded: Vec<Decoded>,
    policy: VerifyPolicy,
    /// Largest byte end reached through the initial R1 context capability on
    /// any verifier-reachable path.  Embeddings use this to bind a program to
    /// a concrete producer ABI instead of manufacturing a padded context.
    required_context_bytes: u32,
}
impl Program {
    pub fn verify(
        instructions: &[Instruction],
        maps: &impl MapResolver,
        helpers: &impl HelperSet,
        policy: VerifyPolicy,
    ) -> Result<Self, VerifyError> {
        let decoded = decode(instructions, maps)?;
        let required_context_bytes = Self::verify_parts(instructions, &decoded, helpers, policy)?;
        Ok(Self {
            instructions: instructions.to_vec(),
            decoded,
            policy,
            required_context_bytes,
        })
    }
    pub fn instructions(&self) -> &[Instruction] {
        &self.instructions
    }
    pub fn decoded(&self) -> &[Decoded] {
        &self.decoded
    }
    pub const fn required_context_bytes(&self) -> u32 {
        self.required_context_bytes
    }
    fn verify_parts(
        insns: &[Instruction],
        decoded: &[Decoded],
        helpers: &impl HelperSet,
        policy: VerifyPolicy,
    ) -> Result<u32, VerifyError> {
        if insns.is_empty() {
            return Err(VerifyError::Empty);
        }
        if insns.len() > policy.max_instructions {
            return Err(VerifyError::TooLong);
        }
        if policy.stack_bytes == 0 {
            return Err(VerifyError::Bounds(0));
        }
        let mut succ = vec![Vec::new(); insns.len()];
        for i in 0..insns.len() {
            if matches!(decoded[i], Decoded::Continuation { .. }) {
                continue;
            }
            validate_opcode(insns[i], decoded[i], i, helpers)?;
            succ[i] = successors(insns, decoded, i)?;
            for &s in &succ[i] {
                if s >= insns.len() || matches!(decoded[s], Decoded::Continuation { .. }) {
                    return Err(VerifyError::BadJump(i));
                }
            }
        }
        if !insns
            .last()
            .map(|x| x.class() == opcode::JMP && x.op() == opcode::EXIT)
            .unwrap_or(false)
        {
            return Err(VerifyError::MissingExit);
        }
        let mut seen = vec![false; insns.len()];
        let mut q = VecDeque::new();
        seen[0] = true;
        q.push_back(0);
        while let Some(i) = q.pop_front() {
            for &n in &succ[i] {
                if !policy.allow_loops && n <= i {
                    return Err(VerifyError::Loop(i));
                }
                if !seen[n] {
                    seen[n] = true;
                    q.push_back(n)
                }
            }
        }
        for i in 0..insns.len() {
            if !matches!(decoded[i], Decoded::Continuation { .. }) && !seen[i] {
                return Err(VerifyError::Unreachable(i));
            }
        }
        let mut state = vec![None; insns.len()];
        state[0] = Some([Ty::Uninit; REGISTER_COUNT]);
        let mut initial = state[0].unwrap();
        initial[1] = Ty::Ptr(
            MemoryRegion::Context,
            0,
            policy.context_bytes,
            policy.context_writable,
        );
        initial[FRAME_POINTER] = Ty::Ptr(
            MemoryRegion::Stack,
            policy.stack_bytes as i32,
            policy.stack_bytes as u32,
            true,
        );
        state[0] = Some(initial);
        let mut work = VecDeque::new();
        work.push_back(0);
        let mut changes = 0;
        let mut required_context_bytes = 0u32;
        while let Some(i) = work.pop_front() {
            let mut s = state[i].unwrap();
            // Record accesses before transfer changes pointer provenance.
            // `check_access` below has already established that the end fits
            // the verifier policy; this is only the producer ABI watermark.
            let access = match insns[i].class() {
                opcode::LDX => match s[insns[i].src() as usize] {
                    Ty::Ptr(MemoryRegion::Context, offset, _, _) => {
                        Some((offset, insns[i].off, size(insns[i])?))
                    }
                    _ => None,
                },
                opcode::ST | opcode::STX => match s[insns[i].dst() as usize] {
                    Ty::Ptr(MemoryRegion::Context, offset, _, _) => {
                        Some((offset, insns[i].off, size(insns[i])?))
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some((base, displacement, width)) = access {
                let end = base
                    .checked_add(displacement as i32)
                    .and_then(|start| start.checked_add(width as i32))
                    .ok_or(VerifyError::Bounds(i))?;
                required_context_bytes = required_context_bytes
                    .max(u32::try_from(end).map_err(|_| VerifyError::Bounds(i))?);
            }
            transfer(insns[i], decoded[i], &mut s, helpers, policy, i)?;
            if insns[i].class() == opcode::JMP && insns[i].op() == opcode::EXIT {
                continue;
            }
            for &n in &succ[i] {
                let s = refine_null_check(
                    insns[i],
                    s,
                    n == (i as i64 + 1 + insns[i].off as i64) as usize,
                );
                let joined = match state[n] {
                    None => s,
                    Some(old) => join(old, s),
                };
                if state[n] != Some(joined) {
                    state[n] = Some(joined);
                    work.push_back(n);
                    changes += 1;
                    if changes > policy.max_states {
                        return Err(VerifyError::StateLimit);
                    }
                }
            }
        }
        Ok(required_context_bytes)
    }
    pub fn execute(
        &self,
        helpers: &mut impl HelperSet,
        context: &mut dyn ExecutionContext,
        step_limit: usize,
    ) -> Result<u64, RuntimeError> {
        Vm::new(self, helpers, context, step_limit).run()
    }
}
fn refine_null_check(
    x: Instruction,
    mut state: [Ty; REGISTER_COUNT],
    taken: bool,
) -> [Ty; REGISTER_COUNT] {
    if !matches!(x.class(), opcode::JMP | opcode::JMP32)
        || x.code & opcode::X != 0
        || x.imm != 0
        || !matches!(x.op(), opcode::JEQ | opcode::JNE)
    {
        return state;
    }
    let dst = x.dst() as usize;
    let Ty::PtrOrNull(region, offset, length, writable) = state[dst] else {
        return state;
    };
    let non_null = (x.op() == opcode::JNE) == taken;
    state[dst] = if non_null {
        Ty::Ptr(region, offset, length, writable)
    } else {
        Ty::Scalar
    };
    state
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ty {
    Uninit,
    Scalar,
    Map(MapRef),
    Ptr(MemoryRegion, i32, u32, bool),
    PtrOrNull(MemoryRegion, i32, u32, bool),
}
fn join(a: [Ty; REGISTER_COUNT], b: [Ty; REGISTER_COUNT]) -> [Ty; REGISTER_COUNT] {
    let mut r = [Ty::Uninit; REGISTER_COUNT];
    for i in 0..REGISTER_COUNT {
        r[i] = if a[i] == b[i] { a[i] } else { Ty::Scalar }
    }
    r
}
fn validate_opcode(
    x: Instruction,
    d: Decoded,
    i: usize,
    h: &impl HelperSet,
) -> Result<(), VerifyError> {
    let c = x.class();
    if !matches!(
        c,
        opcode::LD
            | opcode::LDX
            | opcode::ST
            | opcode::STX
            | opcode::ALU
            | opcode::ALU64
            | opcode::JMP
            | opcode::JMP32
    ) {
        return Err(VerifyError::BadOpcode(i));
    }
    match c {
        opcode::LD if matches!(d, Decoded::WideImmediate(_) | Decoded::Map(_)) => Ok(()),
        // A zero mode is accepted for the crate's pre-existing compact-memory
        // encoding; normal eBPF uses BPF_MEM (0x60).
        opcode::LDX if matches!(x.code & 0xe0, 0 | opcode::MEM) && size(x).is_ok() => Ok(()),
        opcode::ST if matches!(x.code & 0xe0, 0 | opcode::MEM) && size(x).is_ok() => Ok(()),
        opcode::STX if matches!(x.code & 0xe0, 0 | opcode::MEM | ATOMIC) && size(x).is_ok() => {
            if x.code & 0xe0 == ATOMIC && atomic_encoding(x.imm).is_none() {
                Err(VerifyError::BadOpcode(i))
            } else {
                Ok(())
            }
        }
        opcode::ALU | opcode::ALU64 if valid_alu(x) => Ok(()),
        opcode::JMP | opcode::JMP32 if valid_jump(x, c, h, i) => Ok(()),
        _ => Err(VerifyError::BadOpcode(i)),
    }
}
fn valid_alu(x: Instruction) -> bool {
    let op = x.op();
    if op == opcode::NEG {
        return x.code & opcode::X == 0;
    }
    if op == opcode::END {
        return x.class() == opcode::ALU && x.src() == 0 && matches!(x.imm, 16 | 32 | 64);
    }
    matches!(
        op,
        opcode::ADD
            | opcode::SUB
            | opcode::MUL
            | opcode::DIV
            | opcode::OR
            | opcode::AND
            | opcode::LSH
            | opcode::RSH
            | opcode::MOD
            | opcode::XOR
            | opcode::MOV
            | opcode::ARSH
    )
}
fn valid_jump(x: Instruction, class: u8, h: &impl HelperSet, i: usize) -> bool {
    match x.op() {
        // JMP32|JA is the long-jump form: its signed displacement is in imm.
        opcode::JA => match class {
            opcode::JMP => x.code & opcode::X == 0,
            opcode::JMP32 => x.code == opcode::JMP32 | opcode::JA && x.off == 0,
            _ => false,
        },
        opcode::CALL => {
            class == opcode::JMP
                && x.code == opcode::JMP | opcode::CALL
                && h.signature(x.imm as u32).is_some()
        }
        opcode::EXIT => class == opcode::JMP && x.code == opcode::JMP | opcode::EXIT,
        opcode::JEQ
        | opcode::JGT
        | opcode::JGE
        | opcode::JSET
        | opcode::JNE
        | opcode::JSGT
        | opcode::JSGE
        | opcode::JLT
        | opcode::JLE
        | opcode::JSLT
        | opcode::JSLE => true,
        _ => {
            let _ = i;
            false
        }
    }
}
fn atomic_encoding(imm: i32) -> Option<(AtomicOperationCode, bool)> {
    let fetch = imm & FETCH != 0;
    match imm & !FETCH {
        0x00 => Some((AtomicOperationCode::Add, fetch)),
        0x40 => Some((AtomicOperationCode::Or, fetch)),
        0x50 => Some((AtomicOperationCode::And, fetch)),
        0xa0 => Some((AtomicOperationCode::Xor, fetch)),
        0xe0 => Some((AtomicOperationCode::Exchange, fetch)),
        0xf0 if !fetch => Some((AtomicOperationCode::CompareExchange, false)),
        _ => None,
    }
}
#[derive(Clone, Copy)]
enum AtomicOperationCode {
    Add,
    Or,
    And,
    Xor,
    Exchange,
    CompareExchange,
}
fn successors(ins: &[Instruction], d: &[Decoded], i: usize) -> Result<Vec<usize>, VerifyError> {
    let x = ins[i];
    if x.class() == opcode::JMP && x.op() == opcode::EXIT {
        return Ok(Vec::new());
    }
    if x.class() == opcode::JMP || x.class() == opcode::JMP32 {
        if x.op() == opcode::CALL {
            return Ok(vec![i + 1]);
        }
        let displacement = if x.class() == opcode::JMP32 && x.op() == opcode::JA {
            x.imm as i64
        } else {
            x.off as i64
        };
        let t = (i as i64) + 1 + displacement;
        if t < 0 {
            return Err(VerifyError::BadJump(i));
        }
        let t = t as usize;
        if x.op() == opcode::JA {
            return Ok(vec![t]);
        }
        return Ok(vec![i + 1, t]);
    }
    let n = match d[i] {
        Decoded::WideImmediate(_) | Decoded::Map(_) => i + 2,
        _ => i + 1,
    };
    Ok(vec![n])
}
fn transfer(
    x: Instruction,
    d: Decoded,
    s: &mut [Ty; REGISTER_COUNT],
    h: &impl HelperSet,
    p: VerifyPolicy,
    i: usize,
) -> Result<(), VerifyError> {
    let dst = x.dst() as usize;
    let src = x.src() as usize;
    if matches!(
        x.class(),
        opcode::LD | opcode::LDX | opcode::ALU | opcode::ALU64
    ) && dst == FRAME_POINTER
    {
        return Err(VerifyError::Type(i));
    }
    let scalar = |t: Ty| matches!(t, Ty::Scalar);
    match x.class() {
        opcode::LD => {
            s[dst] = match d {
                Decoded::Map(reference) => Ty::Map(reference),
                _ => Ty::Scalar,
            }
        }
        opcode::ALU | opcode::ALU64 => {
            if x.op() == opcode::MOV {
                if x.class() == opcode::ALU
                    && (matches!(s[dst], Ty::Ptr(..) | Ty::PtrOrNull(..))
                        || (x.code & opcode::X != 0
                            && matches!(s[src], Ty::Ptr(..) | Ty::PtrOrNull(..))))
                {
                    return Err(VerifyError::Type(i));
                }
                s[dst] = if x.code & opcode::X != 0 {
                    s[src]
                } else {
                    Ty::Scalar
                }
            } else {
                if s[dst] == Ty::Uninit || (x.code & opcode::X != 0 && s[src] == Ty::Uninit) {
                    return Err(VerifyError::Type(i));
                }
                if let Ty::Ptr(r, o, l, w) = s[dst] {
                    if x.class() == opcode::ALU64
                        && x.op() == opcode::ADD
                        && x.code & opcode::X == 0
                    {
                        let n = o.checked_add(x.imm).ok_or(VerifyError::Bounds(i))?;
                        if n < 0 || n as u32 > l {
                            return Err(VerifyError::Bounds(i));
                        }
                        s[dst] = Ty::Ptr(r, n, l, w)
                    } else {
                        s[dst] = Ty::Scalar
                    }
                } else {
                    s[dst] = Ty::Scalar
                }
            }
        }
        opcode::LDX => {
            let Ty::Ptr(region, off, len, _) = s[src] else {
                return Err(VerifyError::Type(i));
            };
            let width = size(x)?;
            check_access(off, len, x.off, width, false, i)?;
            let field_offset = off
                .checked_add(x.off as i32)
                .and_then(|offset| u32::try_from(offset).ok());
            s[dst] = p
                .context_pointer_fields
                .iter()
                .flatten()
                .find(|field| {
                    region == MemoryRegion::Context
                        && width == u32::from(field.width)
                        && field_offset == Some(field.offset)
                })
                .map_or(Ty::Scalar, |field| {
                    Ty::Ptr(field.region, 0, field.max_length, field.writable)
                });
        }
        opcode::ST | opcode::STX => {
            let Ty::Ptr(_, off, len, w) = s[dst] else {
                return Err(VerifyError::Type(i));
            };
            if !w {
                return Err(VerifyError::Type(i));
            }
            if x.class() == opcode::STX && s[src] == Ty::Uninit {
                return Err(VerifyError::Type(i));
            }
            check_access(off, len, x.off, size(x)?, true, i)?;
            if x.class() == opcode::STX && x.code & 0xe0 == ATOMIC {
                if s[src] != Ty::Scalar
                    || matches!(
                        atomic_encoding(x.imm),
                        Some((AtomicOperationCode::CompareExchange, _))
                    ) && s[0] != Ty::Scalar
                {
                    return Err(VerifyError::Type(i));
                }
                if matches!(atomic_encoding(x.imm), Some((_, true))) {
                    s[0] = Ty::Scalar;
                }
            }
        }
        opcode::JMP | opcode::JMP32 => {
            if x.op() == opcode::CALL {
                let sig = h
                    .signature(x.imm as u32)
                    .ok_or(VerifyError::UnknownHelper(i))?;
                for n in 0..5 {
                    if let Some(k) = sig.args[n] {
                        let v = s[n + 1];
                        let good = match k {
                            ArgKind::Scalar => scalar(v),
                            ArgKind::Map => matches!(v, Ty::Map(_)),
                            ArgKind::Pointer { readable, writable } => {
                                let _ = readable;
                                matches!(v, Ty::Ptr(_, _, _, w) if !writable || w)
                            }
                        };
                        if !good {
                            return Err(VerifyError::Type(i));
                        }
                    }
                }
                let arguments = s.map(|value| match value {
                    Ty::Uninit => HelperArgument::Uninit,
                    Ty::Scalar => HelperArgument::Scalar,
                    Ty::Map(reference) => HelperArgument::Map(reference),
                    Ty::Ptr(region, _, length, writable)
                    | Ty::PtrOrNull(region, _, length, writable) => HelperArgument::Pointer {
                        region,
                        length,
                        writable,
                    },
                });
                let result = h
                    .effects(
                        x.imm as u32,
                        [
                            arguments[1],
                            arguments[2],
                            arguments[3],
                            arguments[4],
                            arguments[5],
                        ],
                    )
                    .ok_or(VerifyError::UnknownHelper(i))?
                    .result;
                s[0] = match result {
                    ReturnKind::Scalar => Ty::Scalar,
                    ReturnKind::MapValueOrNull => {
                        Ty::PtrOrNull(MemoryRegion::MapValue, 0, u32::MAX, true)
                    }
                    ReturnKind::Pointer {
                        region,
                        length,
                        writable,
                    } => Ty::Ptr(region, 0, length, writable),
                    ReturnKind::NullablePointer {
                        region,
                        length,
                        writable,
                    } => Ty::PtrOrNull(region, 0, length, writable),
                };
                // The eBPF calling convention makes argument registers
                // caller-saved, regardless of the helper's declared effects.
                s[1..=5].fill(Ty::Uninit);
            } else if x.op() != opcode::JA
                && x.op() != opcode::EXIT
                && (s[dst] == Ty::Uninit || (x.code & opcode::X != 0 && s[src] == Ty::Uninit))
            {
                return Err(VerifyError::Type(i));
            }
        }
        _ => {}
    }
    let _ = p;
    Ok(())
}
fn size(x: Instruction) -> Result<u32, VerifyError> {
    match x.code & 24 {
        opcode::B => Ok(1),
        opcode::H => Ok(2),
        opcode::W => Ok(4),
        opcode::DW => Ok(8),
        _ => Err(VerifyError::BadOpcode(0)),
    }
}
fn check_access(
    base: i32,
    len: u32,
    off: i16,
    width: u32,
    _write: bool,
    i: usize,
) -> Result<(), VerifyError> {
    let start = base.checked_add(off as i32).ok_or(VerifyError::Bounds(i))?;
    let end = start
        .checked_add(width as i32)
        .ok_or(VerifyError::Bounds(i))?;
    if start < 0 || end as u32 > len {
        Err(VerifyError::Bounds(i))
    } else {
        Ok(())
    }
}

struct Vm<'a, H: HelperSet> {
    p: Program,
    h: &'a mut H,
    c: &'a mut dyn ExecutionContext,
    regs: [Value; REGISTER_COUNT],
    stack: Vec<u8>,
    pc: usize,
    steps: usize,
    limit: usize,
    tail_depth: usize,
}
struct VmHelperMemory<'a> {
    context: &'a mut dyn ExecutionContext,
    stack: &'a mut [u8],
}
impl HelperMemory for VmHelperMemory<'_> {
    fn read(&mut self, capability: Capability, offset: usize, out: &mut [u8]) -> bool {
        if capability.region == MemoryRegion::Stack {
            let end = match offset.checked_add(out.len()) {
                Some(end) => end,
                None => return false,
            };
            if end > self.stack.len() {
                return false;
            }
            out.copy_from_slice(&self.stack[offset..end]);
            true
        } else {
            self.context.read(capability, offset, out)
        }
    }
    fn write(&mut self, capability: Capability, offset: usize, input: &[u8]) -> bool {
        if capability.region == MemoryRegion::Stack {
            let end = match offset.checked_add(input.len()) {
                Some(end) => end,
                None => return false,
            };
            if end > self.stack.len() {
                return false;
            }
            self.stack[offset..end].copy_from_slice(input);
            true
        } else {
            self.context.write(capability, offset, input)
        }
    }
    fn atomic(
        &mut self,
        capability: Capability,
        offset: usize,
        width: u32,
        operation: AtomicOperation,
    ) -> Result<u64, RuntimeError> {
        if capability.region != MemoryRegion::Stack {
            return self.context.atomic(capability, offset, width, operation);
        }
        if !matches!(width, 4 | 8)
            || offset
                .checked_add(width as usize)
                .is_none_or(|end| end > self.stack.len())
        {
            return Err(RuntimeError::Bounds);
        }
        let mut bytes = [0; 8];
        bytes[..width as usize].copy_from_slice(&self.stack[offset..offset + width as usize]);
        let old = u64::from_le_bytes(bytes);
        let mask = if width == 4 {
            u32::MAX as u64
        } else {
            u64::MAX
        };
        let new = match operation {
            AtomicOperation::Add(value) => old.wrapping_add(value),
            AtomicOperation::Or(value) => old | value,
            AtomicOperation::And(value) => old & value,
            AtomicOperation::Xor(value) => old ^ value,
            AtomicOperation::Exchange(value) => value,
            AtomicOperation::CompareExchange { expected, new } if old == expected => new,
            AtomicOperation::CompareExchange { .. } => old,
        } & mask;
        self.stack[offset..offset + width as usize]
            .copy_from_slice(&new.to_le_bytes()[..width as usize]);
        Ok(old)
    }
}
impl<'a, H: HelperSet> Vm<'a, H> {
    fn new(p: &'a Program, h: &'a mut H, c: &'a mut dyn ExecutionContext, limit: usize) -> Self {
        let mut r = [Value::Uninit; REGISTER_COUNT];
        r[1] = Value::Pointer(Capability {
            region: MemoryRegion::Context,
            token: 0,
            offset: 0,
            length: p.policy.context_bytes,
            writable: p.policy.context_writable,
        });
        r[FRAME_POINTER] = Value::Pointer(Capability {
            region: MemoryRegion::Stack,
            token: 0,
            offset: p.policy.stack_bytes as i32,
            length: p.policy.stack_bytes as u32,
            writable: true,
        });
        Self {
            p: p.clone(),
            h,
            c,
            regs: r,
            stack: vec![0; p.policy.stack_bytes],
            pc: 0,
            steps: 0,
            limit,
            tail_depth: 0,
        }
    }
    fn run(mut self) -> Result<u64, RuntimeError> {
        loop {
            if self.steps >= self.limit {
                return Err(RuntimeError::StepLimit);
            }
            self.steps += 1;
            let x = *self
                .p
                .instructions
                .get(self.pc)
                .ok_or(RuntimeError::InvalidProgramCounter)?;
            match x.class() {
                opcode::LD => {
                    if x.dst() as usize == FRAME_POINTER {
                        return Err(RuntimeError::Type);
                    }
                    self.regs[x.dst() as usize] = match self.p.decoded[self.pc] {
                        Decoded::WideImmediate(v) => Value::Scalar(v),
                        Decoded::Map(m) => Value::Map(m),
                        _ => return Err(RuntimeError::Type),
                    };
                    self.pc += 2
                }
                opcode::ALU | opcode::ALU64 => self.alu(x)?,
                opcode::LDX => {
                    if x.dst() as usize == FRAME_POINTER {
                        return Err(RuntimeError::Type);
                    }
                    let v = self.load(x)?;
                    self.regs[x.dst() as usize] = v;
                    self.pc += 1
                }
                opcode::ST | opcode::STX => {
                    if x.class() == opcode::STX && x.code & 0xe0 == ATOMIC {
                        self.atomic(x)?;
                    } else {
                        let v = if x.class() == opcode::ST {
                            x.imm as u32 as u64
                        } else {
                            self.scalar(x.src() as usize)?
                        };
                        self.store(x, v)?;
                        self.pc += 1
                    }
                }
                opcode::JMP | opcode::JMP32 => {
                    if x.op() == opcode::EXIT {
                        return self.scalar(0);
                    }
                    if x.op() == opcode::CALL {
                        let a = [
                            self.regs[1],
                            self.regs[2],
                            self.regs[3],
                            self.regs[4],
                            self.regs[5],
                        ];
                        let effects = self
                            .h
                            .effects(x.imm as u32, a.map(helper_argument_from_value))
                            .ok_or(RuntimeError::Helper)?;
                        if let Some(target) = self.h.tail_call(a)? {
                            // Linux bounds a tail-call chain to 32 and treats
                            // exhaustion as a normal helper failure which
                            // continues in the current program.
                            if self.tail_depth < 32 {
                                self.tail_depth += 1;
                                self.p = target;
                                self.regs = [Value::Uninit; REGISTER_COUNT];
                                self.regs[1] = Value::Pointer(Capability {
                                    region: MemoryRegion::Context,
                                    token: 0,
                                    offset: 0,
                                    length: self.p.policy.context_bytes,
                                    writable: self.p.policy.context_writable,
                                });
                                self.regs[FRAME_POINTER] = Value::Pointer(Capability {
                                    region: MemoryRegion::Stack,
                                    token: 0,
                                    offset: self.p.policy.stack_bytes as i32,
                                    length: self.p.policy.stack_bytes as u32,
                                    writable: true,
                                });
                                self.stack = vec![0; self.p.policy.stack_bytes];
                                self.pc = 0;
                                continue;
                            }
                        }
                        let result = self.h.call(
                            x.imm as u32,
                            a,
                            &mut VmHelperMemory {
                                context: self.c,
                                stack: &mut self.stack,
                            },
                        )?;
                        if !matches_return(result, effects.result) {
                            return Err(RuntimeError::Type);
                        }
                        self.regs[0] = result;
                        self.regs[1..=5].fill(Value::Uninit);
                        self.pc += 1
                    } else if x.op() == opcode::JA || self.condition(x)? {
                        let displacement = if x.class() == opcode::JMP32 && x.op() == opcode::JA {
                            x.imm as i64
                        } else {
                            x.off as i64
                        };
                        self.pc = ((self.pc as i64) + 1 + displacement) as usize
                    } else {
                        self.pc += 1
                    }
                }
                _ => return Err(RuntimeError::Type),
            }
        }
    }
    fn scalar(&self, n: usize) -> Result<u64, RuntimeError> {
        self.regs[n].scalar().ok_or(RuntimeError::Type)
    }
    fn alu(&mut self, x: Instruction) -> Result<(), RuntimeError> {
        let d = x.dst() as usize;
        if d == FRAME_POINTER {
            return Err(RuntimeError::Type);
        }
        if x.op() == opcode::END {
            let a = self.scalar(d)?;
            let r = match x.imm {
                16 => {
                    let v = a as u16;
                    if x.code & opcode::X != 0 {
                        v.swap_bytes() as u64
                    } else {
                        v as u64
                    }
                }
                32 => {
                    let v = a as u32;
                    if x.code & opcode::X != 0 {
                        v.swap_bytes() as u64
                    } else {
                        v as u64
                    }
                }
                64 => {
                    if x.code & opcode::X != 0 {
                        a.swap_bytes()
                    } else {
                        a
                    }
                }
                _ => return Err(RuntimeError::Arithmetic),
            };
            self.regs[d] = Value::Scalar(r);
            self.pc += 1;
            return Ok(());
        }
        if x.op() == opcode::MOV && x.code & opcode::X != 0 {
            if x.class() == opcode::ALU && matches!(self.regs[x.src() as usize], Value::Pointer(_))
            {
                return Err(RuntimeError::Type);
            }
            self.regs[d] = self.regs[x.src() as usize];
            self.pc += 1;
            return Ok(());
        }
        if x.class() == opcode::ALU64 && x.op() == opcode::ADD && x.code & opcode::X == 0 {
            if let Value::Pointer(mut cap) = self.regs[d] {
                cap.offset = cap.offset.checked_add(x.imm).ok_or(RuntimeError::Bounds)?;
                if cap.offset < 0 || cap.offset as u32 > cap.length {
                    return Err(RuntimeError::Bounds);
                }
                self.regs[d] = Value::Pointer(cap);
                self.pc += 1;
                return Ok(());
            }
        }
        let b = if x.code & opcode::X != 0 {
            self.scalar(x.src() as usize)?
        } else if x.class() == opcode::ALU64 {
            x.imm as i64 as u64
        } else {
            x.imm as u32 as u64
        };
        let r = if x.op() == opcode::MOV {
            b
        } else if x.op() == opcode::NEG {
            0u64.wrapping_sub(self.scalar(d)?)
        } else {
            let a = self.scalar(d)?;
            let shift = if x.class() == opcode::ALU {
                b & 31
            } else {
                b & 63
            };
            match x.op() {
                opcode::ADD => a.wrapping_add(b),
                opcode::SUB => a.wrapping_sub(b),
                opcode::MUL => a.wrapping_mul(b),
                opcode::DIV => a.checked_div(b).unwrap_or(0),
                opcode::MOD => a.checked_rem(b).unwrap_or(a),
                opcode::OR => a | b,
                opcode::AND => a & b,
                opcode::LSH => a << shift,
                opcode::RSH => a >> shift,
                opcode::ARSH => ((a as i64) >> shift) as u64,
                opcode::XOR => a ^ b,
                _ => return Err(RuntimeError::Arithmetic),
            }
        };
        self.regs[d] = Value::Scalar(if x.class() == opcode::ALU {
            r as u32 as u64
        } else {
            r
        });
        self.pc += 1;
        Ok(())
    }
    fn atomic(&mut self, x: Instruction) -> Result<(), RuntimeError> {
        let width = size(x).map_err(|_| RuntimeError::Type)?;
        let operand = self.scalar(x.src() as usize)?;
        let (code, fetch) = atomic_encoding(x.imm).ok_or(RuntimeError::Arithmetic)?;
        let expected = if matches!(code, AtomicOperationCode::CompareExchange) {
            Some(self.scalar(0)?)
        } else {
            None
        };
        let op = match code {
            AtomicOperationCode::Add => AtomicOperation::Add(operand),
            AtomicOperationCode::Or => AtomicOperation::Or(operand),
            AtomicOperationCode::And => AtomicOperation::And(operand),
            AtomicOperationCode::Xor => AtomicOperation::Xor(operand),
            AtomicOperationCode::Exchange => AtomicOperation::Exchange(operand),
            AtomicOperationCode::CompareExchange => AtomicOperation::CompareExchange {
                expected: expected.unwrap(),
                new: operand,
            },
        };
        let (capability, offset) = self.cap(x.dst() as usize, x.off, width as usize, true)?;
        let old = if capability.region == MemoryRegion::Stack {
            let mut bytes = [0; 8];
            bytes[..width as usize].copy_from_slice(&self.stack[offset..offset + width as usize]);
            let old = u64::from_le_bytes(bytes);
            let mask = if width == 4 {
                u32::MAX as u64
            } else {
                u64::MAX
            };
            let new = match op {
                AtomicOperation::Add(v) => old.wrapping_add(v),
                AtomicOperation::Or(v) => old | v,
                AtomicOperation::And(v) => old & v,
                AtomicOperation::Xor(v) => old ^ v,
                AtomicOperation::Exchange(v) => v,
                AtomicOperation::CompareExchange { expected, new } if old == expected => new,
                AtomicOperation::CompareExchange { .. } => old,
            } & mask;
            self.stack[offset..offset + width as usize]
                .copy_from_slice(&new.to_le_bytes()[..width as usize]);
            old
        } else {
            self.c.atomic(capability, offset, width, op)?
        };
        if fetch {
            self.regs[x.src() as usize] =
                Value::Scalar(if width == 4 { old as u32 as u64 } else { old });
        }
        if matches!(code, AtomicOperationCode::CompareExchange) {
            self.regs[0] = Value::Scalar(if width == 4 { old as u32 as u64 } else { old });
        }
        self.pc += 1;
        Ok(())
    }
    fn cap(
        &self,
        n: usize,
        off: i16,
        width: usize,
        write: bool,
    ) -> Result<(Capability, usize), RuntimeError> {
        let Value::Pointer(c) = self.regs[n] else {
            return Err(RuntimeError::Type);
        };
        if write && !c.writable {
            return Err(RuntimeError::Type);
        }
        let start = c
            .offset
            .checked_add(off as i32)
            .ok_or(RuntimeError::Bounds)?;
        let end = start
            .checked_add(width as i32)
            .ok_or(RuntimeError::Bounds)?;
        if start < 0 || end as u32 > c.length {
            return Err(RuntimeError::Bounds);
        }
        Ok((c, start as usize))
    }
    fn load(&mut self, x: Instruction) -> Result<Value, RuntimeError> {
        let w = size(x).map_err(|_| RuntimeError::Type)? as usize;
        let (capability, o) = self.cap(x.src() as usize, x.off, w, false)?;
        if let Some(pointer) = self.c.context_pointer(capability, o, w) {
            return Ok(Value::Pointer(pointer));
        }
        let mut b = [0; 8];
        if capability.region == MemoryRegion::Stack {
            b[..w].copy_from_slice(&self.stack[o..o + w])
        } else if !self.c.read(capability, o, &mut b[..w]) {
            return Err(RuntimeError::Memory);
        }
        Ok(Value::Scalar(u64::from_le_bytes(b)))
    }
    fn store(&mut self, x: Instruction, v: u64) -> Result<(), RuntimeError> {
        let w = size(x).map_err(|_| RuntimeError::Type)? as usize;
        let (capability, o) = self.cap(x.dst() as usize, x.off, w, true)?;
        let b = v.to_le_bytes();
        if capability.region == MemoryRegion::Stack {
            self.stack[o..o + w].copy_from_slice(&b[..w]);
            Ok(())
        } else if self.c.write(capability, o, &b[..w]) {
            Ok(())
        } else {
            Err(RuntimeError::Memory)
        }
    }
    fn condition(&self, x: Instruction) -> Result<bool, RuntimeError> {
        let a = self.regs[x.dst() as usize];
        let b = if x.code & opcode::X != 0 {
            self.regs[x.src() as usize]
        } else if x.class() == opcode::JMP {
            Value::Scalar(x.imm as i64 as u64)
        } else {
            Value::Scalar(x.imm as u32 as u64)
        };
        if matches!(x.op(), opcode::JEQ | opcode::JNE) {
            let equal = match (a, b) {
                (Value::Scalar(a), Value::Scalar(b)) => a == b,
                (Value::Pointer(_), Value::Scalar(0)) | (Value::Scalar(0), Value::Pointer(_)) => {
                    false
                }
                (Value::Pointer(a), Value::Pointer(b))
                    if a.region == b.region && a.token == b.token =>
                {
                    a.offset == b.offset
                }
                _ => return Err(RuntimeError::Type),
            };
            return Ok(if x.op() == opcode::JEQ { equal } else { !equal });
        }
        let (a, b) = (
            match a {
                Value::Scalar(value) => value,
                Value::Pointer(capability)
                    if matches!(b, Value::Pointer(other)
                    if capability.region == other.region && capability.token == other.token) =>
                {
                    capability.offset as u64
                }
                _ => return Err(RuntimeError::Type),
            },
            match b {
                Value::Scalar(value) => value,
                Value::Pointer(capability)
                    if matches!(a, Value::Pointer(other)
                    if capability.region == other.region && capability.token == other.token) =>
                {
                    capability.offset as u64
                }
                _ => return Err(RuntimeError::Type),
            },
        );
        let (a32, b32) = (a as u32, b as u32);
        Ok(match x.op() {
            opcode::JGT => {
                if x.class() == opcode::JMP32 {
                    a32 > b32
                } else {
                    a > b
                }
            }
            opcode::JGE => {
                if x.class() == opcode::JMP32 {
                    a32 >= b32
                } else {
                    a >= b
                }
            }
            opcode::JLT => {
                if x.class() == opcode::JMP32 {
                    a32 < b32
                } else {
                    a < b
                }
            }
            opcode::JLE => {
                if x.class() == opcode::JMP32 {
                    a32 <= b32
                } else {
                    a <= b
                }
            }
            opcode::JSET => {
                if x.class() == opcode::JMP32 {
                    a32 & b32 != 0
                } else {
                    a & b != 0
                }
            }
            opcode::JSGT => {
                if x.class() == opcode::JMP32 {
                    (a32 as i32) > (b32 as i32)
                } else {
                    (a as i64) > (b as i64)
                }
            }
            opcode::JSGE => {
                if x.class() == opcode::JMP32 {
                    (a32 as i32) >= (b32 as i32)
                } else {
                    (a as i64) >= (b as i64)
                }
            }
            opcode::JSLT => {
                if x.class() == opcode::JMP32 {
                    (a32 as i32) < (b32 as i32)
                } else {
                    (a as i64) < (b as i64)
                }
            }
            opcode::JSLE => {
                if x.class() == opcode::JMP32 {
                    (a32 as i32) <= (b32 as i32)
                } else {
                    (a as i64) <= (b as i64)
                }
            }
            _ => return Err(RuntimeError::Arithmetic),
        })
    }
}
fn helper_argument_from_value(value: Value) -> HelperArgument {
    match value {
        Value::Uninit => HelperArgument::Uninit,
        Value::Scalar(_) => HelperArgument::Scalar,
        Value::Map(reference) => HelperArgument::Map(reference),
        Value::Pointer(capability) => HelperArgument::Pointer {
            region: capability.region,
            length: capability.length,
            writable: capability.writable,
        },
    }
}
fn matches_return(value: Value, kind: ReturnKind) -> bool {
    match kind {
        ReturnKind::Scalar => matches!(value, Value::Scalar(_)),
        ReturnKind::MapValueOrNull => {
            matches!(value, Value::Scalar(0))
                || matches!(value, Value::Pointer(cap) if cap.region == MemoryRegion::MapValue)
        }
        ReturnKind::Pointer {
            region,
            length,
            writable,
        } => {
            matches!(value, Value::Pointer(cap) if cap.region == region && cap.length <= length && (!writable || cap.writable))
        }
        ReturnKind::NullablePointer {
            region,
            length,
            writable,
        } => {
            matches!(value, Value::Scalar(0))
                || matches!(value, Value::Pointer(cap) if cap.region == region && cap.length <= length && (!writable || cap.writable))
        }
    }
}

/// General fixed-size map primitive; policies such as pinning and handles belong above it.
pub trait Map {
    fn key_size(&self) -> usize;
    fn value_size(&self) -> usize;
    fn lookup(&self, key: &[u8]) -> Option<&[u8]>;
    fn update(&mut self, key: &[u8], value: &[u8]) -> Result<(), MapError>;
    fn remove(&mut self, key: &[u8]) -> bool;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapError {
    KeySize,
    ValueSize,
    Full,
    NoMemory,
}
pub struct ArrayMap {
    value_size: usize,
    values: Vec<u8>,
}
impl ArrayMap {
    pub fn new(entries: usize, value_size: usize) -> Option<Self> {
        entries.checked_mul(value_size).map(|n| Self {
            value_size,
            values: vec![0; n],
        })
    }
}
impl Map for ArrayMap {
    fn key_size(&self) -> usize {
        4
    }
    fn value_size(&self) -> usize {
        self.value_size
    }
    fn lookup(&self, k: &[u8]) -> Option<&[u8]> {
        let i = u32::from_le_bytes(k.try_into().ok()?) as usize;
        let s = i.checked_mul(self.value_size)?;
        self.values.get(s..s + self.value_size)
    }
    fn update(&mut self, k: &[u8], v: &[u8]) -> Result<(), MapError> {
        if v.len() != self.value_size {
            return Err(MapError::ValueSize);
        }
        let i = u32::from_le_bytes(k.try_into().map_err(|_| MapError::KeySize)?) as usize;
        let s = i.checked_mul(self.value_size).ok_or(MapError::Full)?;
        let d = self
            .values
            .get_mut(s..s + self.value_size)
            .ok_or(MapError::Full)?;
        d.copy_from_slice(v);
        Ok(())
    }
    fn remove(&mut self, _: &[u8]) -> bool {
        false
    }
}
pub struct HashMap {
    key_size: usize,
    value_size: usize,
    max: usize,
    /// Fixed at construction.  BPF batch cursors name this topology, never a
    /// transient position in an insertion-order vector.  Each head names a
    /// node in the fixed slot pool.
    buckets: Vec<Option<u32>>,
    /// All key/value backing storage is reserved when the map is created.
    /// Mutations only relink or overwrite these slots, so churn cannot grow
    /// container capacity beyond the map's admission charge.
    slots: Vec<HashNode>,
    /// Fixed-capacity stack of unused slot indexes.
    free: Vec<u32>,
    /// Intrusive recency order. Only LRU users call `touch` and
    /// `replace_lru_full`; the links live in fixed node slots.
    lru_head: Option<u32>,
    lru_tail: Option<u32>,
    entries: usize,
}

struct HashNode {
    key: Vec<u8>,
    value: Vec<u8>,
    next: Option<u32>,
    lru_prev: Option<u32>,
    lru_next: Option<u32>,
    occupied: bool,
}

/// Per-entry fixed storage charged by embedders that create `HashMap`s.
/// Payload bytes are charged separately as `max_entries * (key + value)`.
pub const HASH_MAP_SLOT_BYTES: usize =
    core::mem::size_of::<HashNode>() + core::mem::size_of::<u32>();
/// Per-bucket fixed storage charged by embedders that create `HashMap`s.
pub const HASH_MAP_BUCKET_BYTES: usize = core::mem::size_of::<Option<u32>>();

impl HashNode {
    fn new(key_size: usize, value_size: usize) -> Result<Self, MapError> {
        let mut key = Vec::new();
        key.try_reserve_exact(key_size)
            .map_err(|_| MapError::NoMemory)?;
        let mut value = Vec::new();
        value
            .try_reserve_exact(value_size)
            .map_err(|_| MapError::NoMemory)?;
        Ok(Self {
            key,
            value,
            next: None,
            lru_prev: None,
            lru_next: None,
            occupied: false,
        })
    }
}

struct HashBucketEntries<'a> {
    map: &'a HashMap,
    next: Option<u32>,
}

impl<'a> Iterator for HashBucketEntries<'a> {
    type Item = (&'a Vec<u8>, &'a Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        let index = self.next? as usize;
        let node = &self.map.slots[index];
        self.next = node.next;
        Some((&node.key, &node.value))
    }
}

impl HashMap {
    pub fn new(key_size: usize, value_size: usize, max: usize) -> Result<Self, MapError> {
        if key_size == 0 || value_size == 0 {
            Err(MapError::KeySize)
        } else {
            if max > u32::MAX as usize {
                return Err(MapError::NoMemory);
            }
            let bucket_count = max
                .checked_next_power_of_two()
                .ok_or(MapError::NoMemory)?
                .max(1);
            if bucket_count > u32::MAX as usize {
                return Err(MapError::NoMemory);
            }
            let mut buckets = Vec::new();
            buckets
                .try_reserve_exact(bucket_count)
                .map_err(|_| MapError::NoMemory)?;
            buckets.resize(bucket_count, None);
            let mut slots = Vec::new();
            slots
                .try_reserve_exact(max)
                .map_err(|_| MapError::NoMemory)?;
            for _ in 0..max {
                slots.push(HashNode::new(key_size, value_size)?);
            }
            let mut free = Vec::new();
            free.try_reserve_exact(max)
                .map_err(|_| MapError::NoMemory)?;
            for index in (0..max).rev() {
                free.push(u32::try_from(index).map_err(|_| MapError::NoMemory)?);
            }
            Ok(Self {
                key_size,
                value_size,
                max,
                buckets,
                slots,
                free,
                lru_head: None,
                lru_tail: None,
                entries: 0,
            })
        }
    }
    /// Stable 32-bit batch cursor domain.
    pub fn bucket_count(&self) -> u32 {
        u32::try_from(self.buckets.len()).expect("checked at construction")
    }
    pub fn bucket_for(&self, key: &[u8]) -> u32 {
        // FNV-1a is deterministic across address spaces and resize-free for
        // this map's lifetime.  It is not exposed as a key ordering ABI.
        let mut hash = 0x811c_9dc5u32;
        for byte in key {
            hash = (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193);
        }
        hash & (self.bucket_count() - 1)
    }
    pub fn bucket_entries(
        &self,
        bucket: u32,
    ) -> Option<impl Iterator<Item = (&Vec<u8>, &Vec<u8>)> + '_> {
        Some(HashBucketEntries {
            map: self,
            next: *self.buckets.get(bucket as usize)?,
        })
    }
    pub fn remove_bucket_keys(&mut self, bucket: u32, keys: &[Vec<u8>]) -> bool {
        let bucket = bucket as usize;
        if bucket >= self.buckets.len() {
            return false;
        }
        let mut previous = None;
        let mut current = self.buckets[bucket];
        let mut removed = false;
        while let Some(index) = current {
            let next = self.slots[index as usize].next;
            if keys
                .iter()
                .any(|candidate| candidate == &self.slots[index as usize].key)
            {
                self.unlink(bucket, previous, index, next);
                self.release(index);
                removed = true;
            } else {
                previous = Some(index);
            }
            current = next;
        }
        removed
    }
    /// Replace a full-map victim atomically with a new key/value: every
    /// allocation is completed before the victim becomes removable.
    pub fn replace_full(
        &mut self,
        victim: &[u8],
        key: &[u8],
        value: &[u8],
    ) -> Result<(), MapError> {
        if key.len() != self.key_size || victim.len() != self.key_size {
            return Err(MapError::KeySize);
        }
        if value.len() != self.value_size {
            return Err(MapError::ValueSize);
        }
        let victim_bucket = self.bucket_for(victim) as usize;
        let victim_index = self.find(victim_bucket, victim).ok_or(MapError::Full)?;
        let target = self.bucket_for(key) as usize;
        // The replacement slot's buffers were allocated at construction, so
        // no fallible operation follows unlinking the victim.
        let previous = self.previous(victim_bucket, victim_index);
        let next = self.slots[victim_index as usize].next;
        self.unlink(victim_bucket, previous, victim_index, next);
        self.write_node(victim_index, key, value);
        self.link(target, victim_index);
        Ok(())
    }
    /// Refreshes a stored key to most-recently-used order without allocating.
    pub fn touch(&mut self, key: &[u8]) -> bool {
        let bucket = self.bucket_for(key) as usize;
        let Some(index) = self.find(bucket, key) else {
            return false;
        };
        self.lru_touch(index);
        true
    }
    /// Replaces the least-recently-used entry using its preallocated slot.
    /// After validation there is no allocation or other fallible operation.
    pub fn replace_lru_full(&mut self, key: &[u8], value: &[u8]) -> Result<(), MapError> {
        if key.len() != self.key_size {
            return Err(MapError::KeySize);
        }
        if value.len() != self.value_size {
            return Err(MapError::ValueSize);
        }
        let victim = self.lru_tail.ok_or(MapError::Full)?;
        let victim_bucket = self.bucket_for(&self.slots[victim as usize].key) as usize;
        let target_bucket = self.bucket_for(key) as usize;
        let previous = self.previous(victim_bucket, victim);
        let next = self.slots[victim as usize].next;
        self.unlink(victim_bucket, previous, victim, next);
        self.lru_detach(victim);
        self.write_node(victim, key, value);
        self.link(target_bucket, victim);
        self.lru_link_head(victim);
        Ok(())
    }
    /// Iterates over the currently stored key/value pairs without exposing map
    /// mutation; useful for kernel map iteration adapters.
    pub fn entries(&self) -> impl Iterator<Item = (&Vec<u8>, &Vec<u8>)> {
        self.slots
            .iter()
            .filter(|node| node.occupied)
            .map(|node| (&node.key, &node.value))
    }

    fn find(&self, bucket: usize, key: &[u8]) -> Option<u32> {
        let mut current = self.buckets[bucket];
        while let Some(index) = current {
            let node = &self.slots[index as usize];
            if node.key == key {
                return Some(index);
            }
            current = node.next;
        }
        None
    }
    fn previous(&self, bucket: usize, target: u32) -> Option<u32> {
        let mut previous = None;
        let mut current = self.buckets[bucket];
        while let Some(index) = current {
            if index == target {
                return previous;
            }
            previous = Some(index);
            current = self.slots[index as usize].next;
        }
        None
    }
    fn unlink(&mut self, bucket: usize, previous: Option<u32>, index: u32, next: Option<u32>) {
        if let Some(previous) = previous {
            self.slots[previous as usize].next = next;
        } else {
            self.buckets[bucket] = next;
        }
        self.slots[index as usize].next = None;
    }
    fn link(&mut self, bucket: usize, index: u32) {
        self.slots[index as usize].next = self.buckets[bucket];
        self.buckets[bucket] = Some(index);
    }
    fn write_node(&mut self, index: u32, key: &[u8], value: &[u8]) {
        let node = &mut self.slots[index as usize];
        node.key.clear();
        node.key.extend_from_slice(key);
        node.value.clear();
        node.value.extend_from_slice(value);
        node.occupied = true;
    }
    fn release(&mut self, index: u32) {
        self.lru_detach(index);
        let node = &mut self.slots[index as usize];
        node.key.clear();
        node.value.clear();
        node.next = None;
        node.occupied = false;
        self.free.push(index);
        self.entries -= 1;
    }
    fn lru_touch(&mut self, index: u32) {
        self.lru_detach(index);
        self.lru_link_head(index);
    }
    fn lru_detach(&mut self, index: u32) {
        let (previous, next) = {
            let node = &self.slots[index as usize];
            (node.lru_prev, node.lru_next)
        };
        if let Some(previous) = previous {
            self.slots[previous as usize].lru_next = next;
        } else if self.lru_head == Some(index) {
            self.lru_head = next;
        }
        if let Some(next) = next {
            self.slots[next as usize].lru_prev = previous;
        } else if self.lru_tail == Some(index) {
            self.lru_tail = previous;
        }
        let node = &mut self.slots[index as usize];
        node.lru_prev = None;
        node.lru_next = None;
    }
    fn lru_link_head(&mut self, index: u32) {
        let old_head = self.lru_head;
        self.slots[index as usize].lru_prev = None;
        self.slots[index as usize].lru_next = old_head;
        if let Some(old_head) = old_head {
            self.slots[old_head as usize].lru_prev = Some(index);
        } else {
            self.lru_tail = Some(index);
        }
        self.lru_head = Some(index);
    }
}
impl Map for HashMap {
    fn key_size(&self) -> usize {
        self.key_size
    }
    fn value_size(&self) -> usize {
        self.value_size
    }
    fn lookup(&self, k: &[u8]) -> Option<&[u8]> {
        let index = self.find(self.bucket_for(k) as usize, k)?;
        Some(self.slots[index as usize].value.as_slice())
    }
    fn update(&mut self, k: &[u8], v: &[u8]) -> Result<(), MapError> {
        if k.len() != self.key_size {
            return Err(MapError::KeySize);
        }
        if v.len() != self.value_size {
            return Err(MapError::ValueSize);
        }
        let bucket = self.bucket_for(k) as usize;
        if let Some(index) = self.find(bucket, k) {
            self.slots[index as usize].value.copy_from_slice(v);
            return Ok(());
        }
        if self.entries == self.max {
            return Err(MapError::Full);
        }
        let index = self.free.pop().ok_or(MapError::Full)?;
        self.write_node(index, k, v);
        self.link(bucket, index);
        self.entries += 1;
        Ok(())
    }
    fn remove(&mut self, k: &[u8]) -> bool {
        let bucket = self.bucket_for(k) as usize;
        if let Some(index) = self.find(bucket, k) {
            let previous = self.previous(bucket, index);
            let next = self.slots[index as usize].next;
            self.unlink(bucket, previous, index, next);
            self.release(index);
            true
        } else {
            false
        }
    }
}
/// Bounded FIFO records, suitable for domain adapters that need ring semantics.
pub struct RingBuffer {
    capacity: usize,
    used: usize,
    records: VecDeque<Vec<u8>>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RingError {
    Full,
    TooLarge,
}
impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            used: 0,
            records: VecDeque::new(),
        }
    }
    pub fn push(&mut self, r: &[u8]) -> Result<(), RingError> {
        if r.len() > self.capacity {
            return Err(RingError::TooLarge);
        }
        if self.used + r.len() > self.capacity {
            return Err(RingError::Full);
        }
        self.used += r.len();
        self.records.push_back(r.to_vec());
        Ok(())
    }
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let x = self.records.pop_front()?;
        self.used -= x.len();
        Some(x)
    }
    pub fn used(&self) -> usize {
        self.used
    }
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests {
    use super::*;
    struct Maps;
    impl MapResolver for Maps {
        fn resolve(&self, r: MapRef) -> Option<MapInfo> {
            (r.0 == 7).then_some(MapInfo {
                key_size: 4,
                value_size: 8,
                max_entries: 1,
            })
        }
    }
    struct H;
    impl HelperSet for H {
        fn signature(&self, _: u32) -> Option<HelperSignature> {
            None
        }
        fn call(
            &mut self,
            _: u32,
            _: [Value; 5],
            _: &mut dyn HelperMemory,
        ) -> Result<Value, RuntimeError> {
            Err(RuntimeError::Helper)
        }
    }
    struct C;
    impl ExecutionContext for C {
        fn read(&mut self, _: Capability, _: usize, _: &mut [u8]) -> bool {
            false
        }
        fn write(&mut self, _: Capability, _: usize, _: &[u8]) -> bool {
            false
        }
    }
    fn i(code: u8, regs: u8, off: i16, imm: i32) -> Instruction {
        Instruction {
            code,
            regs,
            off,
            imm,
        }
    }
    #[test]
    fn decoder_resolves_and_rejects_wide_tail() {
        assert!(matches!(
            decode(
                &[i(opcode::LD | opcode::DW, 0x10, 0, 7), i(0, 0, 0, 0)],
                &Maps
            )
            .unwrap()[0],
            Decoded::Map(MapRef(7))
        ));
        assert!(decode(&[i(opcode::LD | opcode::DW, 0, 0, 0)], &Maps).is_err())
    }
    #[test]
    fn cfg_rejects_back_edge() {
        let x = [
            i(opcode::JMP | opcode::JA, 0, -1, 0),
            i(opcode::JMP | opcode::EXIT, 0, 0, 0),
        ];
        assert!(matches!(
            Program::verify(&x, &Maps, &H, VerifyPolicy::default()),
            Err(VerifyError::Loop(_))
        ))
    }
    #[test]
    fn termination_and_stack_pointer_execute() {
        let x = [
            i(opcode::ALU64 | opcode::MOV, 0, 0, 9),
            i(opcode::STX | opcode::W, 10, -4, 0),
            i(opcode::LDX | opcode::W, 0xa0, -4, 0),
            i(opcode::JMP | opcode::EXIT, 0, 0, 0),
        ];
        let p = Program::verify(&x, &Maps, &H, VerifyPolicy::default()).unwrap();
        let mut h = H;
        let mut c = C;
        let mut vm = Vm::new(&p, &mut h, &mut c, 10);
        assert!(vm.alu(x[0]).is_ok());
        assert!(vm.store(x[1], 9).is_ok());
        assert_eq!(vm.load(x[2]), Ok(Value::Scalar(9)));
        assert_eq!(p.execute(&mut H, &mut C, 10), Ok(9))
    }
    #[test]
    fn maps_and_ring_are_bounded() {
        let mut h = HashMap::new(1, 2, 1).unwrap();
        h.update(&[1], &[2, 3]).unwrap();
        assert_eq!(h.lookup(&[1]), Some(&[2, 3][..]));
        let mut r = RingBuffer::new(3);
        r.push(&[1, 2]).unwrap();
        assert_eq!(r.push(&[3, 4]), Err(RingError::Full));
        assert_eq!(r.pop(), Some(vec![1, 2]))
    }
    #[test]
    fn extended_alu_and_jmp32_semantics_execute() {
        let alu = [
            i(opcode::ALU64 | opcode::MOV, 0, 0, -8),
            i(opcode::ALU64 | opcode::ARSH, 0, 0, 1),
            i(opcode::ALU64 | opcode::NEG, 0, 0, 0),
            i(opcode::ALU | opcode::END | opcode::X, 0, 0, 16),
            i(opcode::JMP | opcode::EXIT, 0, 0, 0),
        ];
        let p = Program::verify(&alu, &Maps, &H, VerifyPolicy::default()).unwrap();
        // -8 >> 1, negated, then converted to big-endian u16: 0x0400.
        assert_eq!(p.execute(&mut H, &mut C, 16), Ok(0x400));

        let jmp32 = [
            i(opcode::ALU64 | opcode::MOV, 0, 0, -1),
            i(opcode::JMP32 | opcode::JSGT, 0, 1, 0),
            i(opcode::ALU64 | opcode::MOV, 0, 0, 7),
            i(opcode::JMP | opcode::EXIT, 0, 0, 0),
        ];
        let p = Program::verify(&jmp32, &Maps, &H, VerifyPolicy::default()).unwrap();
        assert_eq!(p.execute(&mut H, &mut C, 16), Ok(7));
    }
    #[test]
    fn atomic_stack_add_executes_as_rmw() {
        let x = [
            i(opcode::ALU64 | opcode::MOV | opcode::X, 0xa1, 0, 0),
            i(opcode::ST | opcode::W, 1, -4, 4),
            i(opcode::ALU64 | opcode::MOV, 2, 0, 3),
            i(opcode::STX | opcode::W | ATOMIC, 0x21, -4, 0),
            i(opcode::LDX | opcode::W, 0x10, -4, 0),
            i(opcode::JMP | opcode::EXIT, 0, 0, 0),
        ];
        let p = Program::verify(&x, &Maps, &H, VerifyPolicy::default()).unwrap();
        assert_eq!(p.execute(&mut H, &mut C, 16), Ok(7));
    }
    #[test]
    fn atomic_fetch_returns_old_value_in_source_register() {
        let x = [
            i(opcode::ALU64 | opcode::MOV | opcode::X, 0xa1, 0, 0),
            i(opcode::ST | opcode::W, 1, -4, 4),
            i(opcode::ALU64 | opcode::MOV, 2, 0, 3),
            i(opcode::STX | opcode::W | ATOMIC, 0x21, -4, FETCH),
            i(opcode::ALU64 | opcode::MOV | opcode::X, 0x20, 0, 0),
            i(opcode::JMP | opcode::EXIT, 0, 0, 0),
        ];
        let p = Program::verify(&x, &Maps, &H, VerifyPolicy::default()).unwrap();
        assert_eq!(p.execute(&mut H, &mut C, 16), Ok(4));
    }
    #[test]
    fn helper_memory_can_access_stack_and_calls_clobber_arguments() {
        struct StackHelper;
        impl HelperSet for StackHelper {
            fn signature(&self, id: u32) -> Option<HelperSignature> {
                (id == 1).then_some(HelperSignature {
                    args: [
                        None,
                        Some(ArgKind::Pointer {
                            readable: false,
                            writable: true,
                        }),
                        None,
                        None,
                        None,
                    ],
                    result: ReturnKind::Scalar,
                })
            }
            fn call(
                &mut self,
                _: u32,
                args: [Value; 5],
                memory: &mut dyn HelperMemory,
            ) -> Result<Value, RuntimeError> {
                let Value::Pointer(capability) = args[1] else {
                    return Err(RuntimeError::Type);
                };
                if !memory.write(
                    capability,
                    (capability.offset - 4) as usize,
                    &9u32.to_le_bytes(),
                ) {
                    return Err(RuntimeError::Memory);
                }
                Ok(Value::Scalar(0))
            }
        }
        let x = [
            i(opcode::ALU64 | opcode::MOV | opcode::X, 0xa2, 0, 0),
            i(opcode::JMP | opcode::CALL, 0, 0, 1),
            i(opcode::LDX | opcode::W, 0xa0, -4, 0),
            i(opcode::JMP | opcode::EXIT, 0, 0, 0),
        ];
        let p = Program::verify(&x, &Maps, &StackHelper, VerifyPolicy::default()).unwrap();
        assert_eq!(p.execute(&mut StackHelper, &mut C, 16), Ok(9));

        let uses_clobbered_r1 = [
            i(opcode::ALU64 | opcode::MOV | opcode::X, 0xa2, 0, 0),
            i(opcode::JMP | opcode::CALL, 0, 0, 1),
            i(opcode::LDX | opcode::W, 0x10, 0, 0),
            i(opcode::JMP | opcode::EXIT, 0, 0, 0),
        ];
        assert!(matches!(
            Program::verify(
                &uses_clobbered_r1,
                &Maps,
                &StackHelper,
                VerifyPolicy::default()
            ),
            Err(VerifyError::Type(2))
        ));
    }
    #[test]
    fn frame_pointer_is_read_only_and_jmp32_ja_uses_imm() {
        let writes_r10 = [
            i(opcode::ALU64 | opcode::MOV, 10, 0, 1),
            i(opcode::JMP | opcode::EXIT, 0, 0, 0),
        ];
        assert!(matches!(
            Program::verify(&writes_r10, &Maps, &H, VerifyPolicy::default()),
            Err(VerifyError::Type(0))
        ));
        let jump = [
            i(opcode::JMP32 | opcode::JA, 0, 0, 0),
            i(opcode::JMP | opcode::EXIT, 0, 0, 0),
        ];
        assert!(Program::verify(&jump, &Maps, &H, VerifyPolicy::default()).is_ok());
    }
    #[test]
    fn helper_effects_receive_the_specific_map_argument() {
        struct Dynamic(core::cell::Cell<Option<MapRef>>);
        impl HelperSet for Dynamic {
            fn signature(&self, id: u32) -> Option<HelperSignature> {
                (id == 1).then_some(HelperSignature {
                    args: [Some(ArgKind::Map), None, None, None, None],
                    result: ReturnKind::MapValueOrNull,
                })
            }
            fn effects(&self, _: u32, args: [HelperArgument; 5]) -> Option<HelperEffects> {
                let HelperArgument::Map(reference) = args[0] else {
                    return None;
                };
                self.0.set(Some(reference));
                Some(HelperEffects {
                    result: ReturnKind::Scalar,
                })
            }
            fn call(
                &mut self,
                _: u32,
                _: [Value; 5],
                _: &mut dyn HelperMemory,
            ) -> Result<Value, RuntimeError> {
                Ok(Value::Scalar(3))
            }
        }
        let x = [
            i(opcode::LD | opcode::DW, 0x11, 0, 7),
            i(0, 0, 0, 0),
            i(opcode::JMP | opcode::CALL, 0, 0, 1),
            i(opcode::JMP | opcode::EXIT, 0, 0, 0),
        ];
        let helper = Dynamic(core::cell::Cell::new(None));
        let p = Program::verify(&x, &Maps, &helper, VerifyPolicy::default()).unwrap();
        assert_eq!(helper.0.get(), Some(MapRef(7)));
        assert_eq!(
            p.execute(&mut Dynamic(core::cell::Cell::new(None)), &mut C, 16),
            Ok(3)
        );
    }
    #[test]
    fn nullable_pointer_can_be_tested_before_use() {
        struct Nullable;
        impl HelperSet for Nullable {
            fn signature(&self, id: u32) -> Option<HelperSignature> {
                (id == 1).then_some(HelperSignature {
                    args: [None; 5],
                    result: ReturnKind::NullablePointer {
                        region: MemoryRegion::RingReservation,
                        length: 8,
                        writable: true,
                    },
                })
            }
            fn call(
                &mut self,
                _: u32,
                _: [Value; 5],
                _: &mut dyn HelperMemory,
            ) -> Result<Value, RuntimeError> {
                Ok(Value::Pointer(Capability {
                    region: MemoryRegion::RingReservation,
                    token: 42,
                    offset: 0,
                    length: 8,
                    writable: true,
                }))
            }
        }
        let x = [
            i(opcode::JMP | opcode::CALL, 0, 0, 1),
            i(opcode::JMP | opcode::JEQ, 0, 1, 0),
            i(opcode::ALU64 | opcode::MOV, 0, 0, 9),
            i(opcode::JMP | opcode::EXIT, 0, 0, 0),
        ];
        let p = Program::verify(&x, &Maps, &Nullable, VerifyPolicy::default()).unwrap();
        assert_eq!(p.execute(&mut Nullable, &mut C, 16), Ok(9));
    }
}
