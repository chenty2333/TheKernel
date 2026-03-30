//! eBPF program verifier.
//!
//! Validates BPF programs before execution to keep the verifier and VM on the
//! same instruction semantics. Performs:
//! 1. Structural validation and decoding of wide instructions
//! 2. Map-fd resolution for pseudo immediates
//! 3. CFG / DAG check (no loops)
//! 4. Abstract interpretation with sound register-state joins

use alloc::{collections::VecDeque, string::String, sync::Arc, vec, vec::Vec};

use axerrno::{AxError, AxResult};

use super::{defs::*, helpers::HelperMemMask, map::BpfMap, prog::uses_raw_ctx_prog_type};
use crate::file::{FileLike, bpf::BpfMapFd};

// ---------------------------------------------------------------------------
// Register value types for abstract interpretation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegType {
    /// Register has not been written to.
    Uninit,
    /// Register holds a scalar (numeric) value.
    Scalar,
    /// Register holds a pointer into the BPF stack (R10-based).
    StackPtr,
    /// Register holds a pointer into a context buffer.
    CtxPtr,
    /// Register holds a pointer into a map value (returned by map_lookup_elem).
    MapValuePtr,
    /// Register holds a nullable pointer returned by map_lookup_elem.
    MapValueOrNull,
    /// Register holds a map pointer (used as argument to helpers).
    MapPtr,
}

#[derive(Debug, Clone, Copy)]
struct RegState {
    ty: RegType,
}

impl Default for RegState {
    fn default() -> Self {
        Self {
            ty: RegType::Uninit,
        }
    }
}

impl RegState {
    fn scalar() -> Self {
        Self {
            ty: RegType::Scalar,
        }
    }

    fn ctx() -> Self {
        Self {
            ty: RegType::CtxPtr,
        }
    }

    fn stack() -> Self {
        Self {
            ty: RegType::StackPtr,
        }
    }

    fn map_value() -> Self {
        Self {
            ty: RegType::MapValuePtr,
        }
    }

    fn map_value_or_null() -> Self {
        Self {
            ty: RegType::MapValueOrNull,
        }
    }

    fn map_ptr() -> Self {
        Self {
            ty: RegType::MapPtr,
        }
    }

    fn is_init(&self) -> bool {
        self.ty != RegType::Uninit
    }

    fn is_ptr(&self) -> bool {
        matches!(
            self.ty,
            RegType::StackPtr
                | RegType::CtxPtr
                | RegType::MapValuePtr
                | RegType::MapValueOrNull
                | RegType::MapPtr
        )
    }

    fn is_mem_ptr(&self) -> bool {
        matches!(
            self.ty,
            RegType::StackPtr | RegType::CtxPtr | RegType::MapValuePtr
        )
    }
}

// ---------------------------------------------------------------------------
// Verifier log
// ---------------------------------------------------------------------------

struct VerifierLog {
    buf: String,
    enabled: bool,
}

impl VerifierLog {
    fn new(enabled: bool) -> Self {
        Self {
            buf: String::new(),
            enabled,
        }
    }

    fn log(&mut self, msg: &str) {
        if self.enabled {
            self.buf.push_str(msg);
            self.buf.push('\n');
        }
    }
}

// ---------------------------------------------------------------------------
// Verifier context
// ---------------------------------------------------------------------------

/// Result of successful verification.
pub struct VerifiedProgram {
    /// Maps referenced by the program (resolved from fd immediates).
    pub maps: Vec<Arc<dyn BpfMap>>,
    /// The original instruction stream.
    pub insns: Vec<BpfInsn>,
    /// Decoded instruction metadata shared with the VM.
    pub decoded_insns: Vec<BpfInsnAux>,
    /// Verifier log output.
    pub log: String,
}

/// Verification failure with optional log output.
pub struct VerifierFailure {
    pub err: AxError,
    pub log: String,
}

macro_rules! try_with_log {
    ($expr:expr, $log:ident) => {
        match $expr {
            Ok(value) => value,
            Err(err) => {
                return Err(VerifierFailure {
                    err,
                    log: $log.buf,
                });
            }
        }
    };
}

/// Verify a BPF program.
///
/// `insns` is the raw instruction stream from user space.
/// `log_level` > 0 enables the verifier log.
pub fn verify_program(
    insns: &[BpfInsn],
    prog_type: u32,
    log_level: u32,
) -> Result<VerifiedProgram, VerifierFailure> {
    let mut log = VerifierLog::new(log_level > 0);

    if !uses_raw_ctx_prog_type(prog_type) {
        log.log("unsupported program type for current verifier/VM model");
        return Err(VerifierFailure {
            err: AxError::InvalidInput,
            log: log.buf,
        });
    }

    if insns.is_empty() || insns.len() > BPF_MAX_INSNS {
        log.log("program length out of range");
        return Err(VerifierFailure {
            err: AxError::InvalidInput,
            log: log.buf,
        });
    }

    // Pass 0: Decode raw instructions into a verifier/VM-shared shape.
    let mut decoded_insns = try_with_log!(decode_program(insns, &mut log), log);

    // Pass 1: Structural validation.
    try_with_log!(pass_structural(insns, &decoded_insns, &mut log), log);

    // Pass 2: Resolve map fd references in LD_IMM_DW instructions.
    let maps = try_with_log!(pass_resolve_maps(&mut decoded_insns, &mut log), log);

    // Pass 3: CFG / DAG check (no loops).
    try_with_log!(pass_cfg(insns, &decoded_insns, &mut log), log);

    // Pass 4: Abstract interpretation (register state tracking).
    try_with_log!(pass_abstract_interp(insns, &decoded_insns, &mut log), log);

    Ok(VerifiedProgram {
        maps,
        insns: insns.to_vec(),
        decoded_insns,
        log: log.buf,
    })
}

// ---------------------------------------------------------------------------
// Pass 0: Decode and normalize the instruction stream
// ---------------------------------------------------------------------------

fn decode_program(insns: &[BpfInsn], log: &mut VerifierLog) -> AxResult<Vec<BpfInsnAux>> {
    let mut decoded = vec![BpfInsnAux::Basic; insns.len()];
    let mut i = 0;

    while i < insns.len() {
        let insn = &insns[i];

        if insn.dst_reg() > 10 || insn.src_reg() > 10 {
            log.log(&alloc::format!("insn {i}: invalid register index"));
            return Err(AxError::InvalidInput);
        }

        if is_ld_imm64_candidate(insn) {
            let next = insns.get(i + 1).ok_or_else(|| {
                log.log(&alloc::format!("insn {i}: LD_IMM_DW at end of program"));
                AxError::InvalidInput
            })?;

            if next.code != 0 || next.regs != 0 || next.off != 0 {
                log.log(&alloc::format!(
                    "insn {}: invalid LD_IMM_DW continuation encoding",
                    i + 1
                ));
                return Err(AxError::InvalidInput);
            }

            let imm64 = (insn.imm as u32 as u64) | ((next.imm as u32 as u64) << 32);
            let data = match insn.src_reg() {
                0 => BpfLdImm64Data::Immediate(imm64),
                BPF_PSEUDO_MAP_FD => BpfLdImm64Data::MapFd(insn.imm),
                BPF_PSEUDO_MAP_VALUE => {
                    log.log(&alloc::format!(
                        "insn {i}: BPF_PSEUDO_MAP_VALUE is not supported"
                    ));
                    return Err(AxError::InvalidInput);
                }
                _ => {
                    log.log(&alloc::format!(
                        "insn {i}: unsupported LD_IMM_DW pseudo src_reg {}",
                        insn.src_reg()
                    ));
                    return Err(AxError::InvalidInput);
                }
            };

            decoded[i] = BpfInsnAux::LdImm64Head(data);
            decoded[i + 1] = BpfInsnAux::LdImm64Cont { head: i };
            i += 2;
            continue;
        }

        i += 1;
    }

    Ok(decoded)
}

// ---------------------------------------------------------------------------
// Pass 1: Structural validation
// ---------------------------------------------------------------------------

fn pass_structural(
    insns: &[BpfInsn],
    decoded: &[BpfInsnAux],
    log: &mut VerifierLog,
) -> AxResult<()> {
    for (i, insn) in insns.iter().enumerate() {
        if decoded[i].is_continuation() {
            continue;
        }

        validate_supported_opcode(insn, decoded[i], i, log)?;

        if writes_dst_reg(insn, decoded[i]) && insn.dst_reg() == BPF_REG_FP as u8 {
            log.log(&alloc::format!("insn {i}: write to R10 (frame pointer)"));
            return Err(AxError::InvalidInput);
        }
        if writes_src_reg(insn) && insn.src_reg() == BPF_REG_FP as u8 {
            log.log(&alloc::format!("insn {i}: write to R10 (frame pointer)"));
            return Err(AxError::InvalidInput);
        }

        match insn.class() {
            BPF_CLASS_LD => {
                if !matches!(decoded[i], BpfInsnAux::LdImm64Head(_)) {
                    log.log(&alloc::format!(
                        "insn {i}: unsupported LD-class opcode {:#x}",
                        insn.code
                    ));
                    return Err(AxError::InvalidInput);
                }
            }
            BPF_CLASS_JMP | BPF_CLASS_JMP32 => {
                let op = insn.op();

                if insn.class() == BPF_CLASS_JMP32 && matches!(op, BPF_OP_CALL | BPF_OP_EXIT) {
                    log.log(&alloc::format!("insn {i}: invalid JMP32 opcode {op:#x}"));
                    return Err(AxError::InvalidInput);
                }

                if matches!(op, BPF_OP_CALL | BPF_OP_EXIT) {
                    continue;
                }

                let target = calc_jump_target(i, insn);
                validate_jump_target(decoded, target, i, log)?;
            }
            _ => {}
        }
    }

    let last = &insns[insns.len() - 1];
    if decoded[insns.len() - 1].is_continuation()
        || last.class() != BPF_CLASS_JMP
        || last.op() != BPF_OP_EXIT
    {
        log.log("program does not end with EXIT");
        return Err(AxError::InvalidInput);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Pass 2: Resolve map fd references
// ---------------------------------------------------------------------------

fn pass_resolve_maps(
    decoded: &mut [BpfInsnAux],
    log: &mut VerifierLog,
) -> AxResult<Vec<Arc<dyn BpfMap>>> {
    let mut maps: Vec<Arc<dyn BpfMap>> = Vec::new();

    for (i, aux) in decoded.iter_mut().enumerate() {
        let BpfInsnAux::LdImm64Head(BpfLdImm64Data::MapFd(map_fd)) = *aux else {
            continue;
        };

        let map_fd_obj = BpfMapFd::from_fd(map_fd).map_err(|_| {
            log.log(&alloc::format!("insn {i}: invalid map fd {map_fd}"));
            AxError::BadFileDescriptor
        })?;

        let map_index = maps
            .iter()
            .position(|map| map.id() == map_fd_obj.map.id())
            .unwrap_or_else(|| {
                let idx = maps.len();
                maps.push(map_fd_obj.map.clone());
                idx
            });

        *aux = BpfInsnAux::LdImm64Head(BpfLdImm64Data::MapIndex(map_index as u32));
    }

    Ok(maps)
}

// ---------------------------------------------------------------------------
// Pass 3: CFG / DAG check — no backward jumps (no loops)
// ---------------------------------------------------------------------------

fn pass_cfg(insns: &[BpfInsn], decoded: &[BpfInsnAux], log: &mut VerifierLog) -> AxResult<()> {
    let n = insns.len();
    let mut succs: Vec<Vec<usize>> = vec![Vec::new(); n];

    for i in 0..n {
        if decoded[i].is_continuation() {
            continue;
        }
        succs[i] = insn_successors(insns, decoded, i)?;
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    let mut color = vec![Color::White; n];
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    color[0] = Color::Gray;

    while let Some((node, succ_idx)) = stack.last_mut() {
        if *succ_idx >= succs[*node].len() {
            color[*node] = Color::Black;
            stack.pop();
            continue;
        }

        let next = succs[*node][*succ_idx];
        *succ_idx += 1;
        match color[next] {
            Color::Gray => {
                log.log(&alloc::format!(
                    "back edge detected: {} -> {} (loop)",
                    *node,
                    next
                ));
                return Err(AxError::InvalidInput);
            }
            Color::White => {
                color[next] = Color::Gray;
                stack.push((next, 0));
            }
            Color::Black => {}
        }
    }

    for i in 0..n {
        if decoded[i].is_continuation() {
            continue;
        }
        if color[i] == Color::White {
            log.log(&alloc::format!("insn {i}: unreachable"));
            return Err(AxError::InvalidInput);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Pass 4: Abstract interpretation — register state tracking
// ---------------------------------------------------------------------------

fn pass_abstract_interp(
    insns: &[BpfInsn],
    decoded: &[BpfInsnAux],
    log: &mut VerifierLog,
) -> AxResult<()> {
    let n = insns.len();
    let mut states: Vec<Option<[RegState; BPF_MAX_REGS]>> = vec![None; n];

    let mut init = [RegState::default(); BPF_MAX_REGS];
    init[1] = RegState::ctx();
    init[BPF_REG_FP] = RegState::stack();
    states[0] = Some(init);

    let mut worklist = VecDeque::new();
    worklist.push_back(0usize);

    while let Some(i) = worklist.pop_front() {
        if decoded[i].is_continuation() {
            log.log(&alloc::format!(
                "insn {i}: control reached LD_IMM_DW continuation"
            ));
            return Err(AxError::InvalidInput);
        }

        let mut regs = states[i].unwrap();
        let insn = &insns[i];
        let dst = insn.dst_reg() as usize;
        let src = insn.src_reg() as usize;

        match insn.class() {
            BPF_CLASS_ALU | BPF_CLASS_ALU64 => {
                let op = insn.op();
                if op == BPF_OP_MOV {
                    if insn.code & BPF_SRC_X != 0 {
                        check_reg_init(&regs, src, i, log)?;
                        regs[dst] = if regs[src].is_ptr() {
                            regs[src]
                        } else {
                            RegState::scalar()
                        };
                    } else {
                        regs[dst] = RegState::scalar();
                    }
                } else if op == BPF_OP_NEG {
                    check_reg_init(&regs, dst, i, log)?;
                    regs[dst] = RegState::scalar();
                } else {
                    check_reg_init(&regs, dst, i, log)?;
                    if insn.code & BPF_SRC_X != 0 {
                        check_reg_init(&regs, src, i, log)?;
                    }
                    if preserves_mem_ptr(insn, regs[dst].ty) {
                        // Preserve pointer provenance for 64-bit add/sub by
                        // an immediate. Runtime bounds checks still validate
                        // the resulting address before any memory access.
                    } else {
                        regs[dst] = RegState::scalar();
                    }
                }
            }
            BPF_CLASS_LDX => {
                check_reg_init(&regs, src, i, log)?;
                if !regs[src].is_mem_ptr() {
                    log.log(&alloc::format!("insn {i}: LDX src R{src} is not a pointer"));
                    return Err(AxError::InvalidInput);
                }
                regs[dst] = RegState::scalar();
            }
            BPF_CLASS_STX => {
                check_reg_init(&regs, dst, i, log)?;
                check_reg_init(&regs, src, i, log)?;
                if !regs[dst].is_mem_ptr() {
                    log.log(&alloc::format!("insn {i}: STX dst R{dst} is not a pointer"));
                    return Err(AxError::InvalidInput);
                }
                if !mem_ptr_writable(regs[dst].ty) {
                    log.log(&alloc::format!("insn {i}: STX dst R{dst} is not writable"));
                    return Err(AxError::InvalidInput);
                }
                if (insn.code & 0xe0) == BPF_MODE_ATOMIC {
                    verify_atomic(&mut regs, insn, i, log)?;
                }
            }
            BPF_CLASS_ST => {
                check_reg_init(&regs, dst, i, log)?;
                if !regs[dst].is_mem_ptr() {
                    log.log(&alloc::format!("insn {i}: ST dst R{dst} is not a pointer"));
                    return Err(AxError::InvalidInput);
                }
                if !mem_ptr_writable(regs[dst].ty) {
                    log.log(&alloc::format!("insn {i}: ST dst R{dst} is not writable"));
                    return Err(AxError::InvalidInput);
                }
            }
            BPF_CLASS_LD => match decoded[i] {
                BpfInsnAux::LdImm64Head(BpfLdImm64Data::Immediate(_)) => {
                    regs[dst] = RegState::scalar();
                }
                BpfInsnAux::LdImm64Head(BpfLdImm64Data::MapIndex(_)) => {
                    regs[dst] = RegState::map_ptr();
                }
                BpfInsnAux::LdImm64Head(BpfLdImm64Data::MapFd(_)) => {
                    log.log(&alloc::format!("insn {i}: unresolved map fd immediate"));
                    return Err(AxError::InvalidInput);
                }
                _ => {
                    log.log(&alloc::format!(
                        "insn {i}: unsupported LD-class instruction"
                    ));
                    return Err(AxError::InvalidInput);
                }
            },
            BPF_CLASS_JMP | BPF_CLASS_JMP32 => {
                let op = insn.op();
                if op == BPF_OP_EXIT {
                    check_reg_init(&regs, 0, i, log)?;
                    continue;
                }

                if op == BPF_OP_CALL {
                    verify_call(&mut regs, insn.imm as u32, i, log)?;
                } else if op != BPF_OP_JA {
                    check_reg_init(&regs, dst, i, log)?;
                    if insn.code & BPF_SRC_X != 0 {
                        check_reg_init(&regs, src, i, log)?;
                    }
                }
            }
            _ => {
                log.log(&alloc::format!(
                    "insn {i}: unsupported instruction class {:#x}",
                    insn.class()
                ));
                return Err(AxError::InvalidInput);
            }
        }

        for (succ, succ_regs) in successor_states(insns, decoded, i, &regs)? {
            if merge_state(&mut states, succ, &succ_regs) {
                worklist.push_back(succ);
            }
        }
    }

    Ok(())
}

fn check_reg_init(
    regs: &[RegState; BPF_MAX_REGS],
    reg: usize,
    insn_idx: usize,
    log: &mut VerifierLog,
) -> AxResult<()> {
    if !regs[reg].is_init() {
        log.log(&alloc::format!("insn {insn_idx}: R{reg} is uninitialized"));
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn merge_state(
    states: &mut [Option<[RegState; BPF_MAX_REGS]>],
    target: usize,
    incoming: &[RegState; BPF_MAX_REGS],
) -> bool {
    let Some(existing) = &mut states[target] else {
        states[target] = Some(*incoming);
        return true;
    };

    let mut changed = false;
    for reg in 0..BPF_MAX_REGS {
        let merged = join_reg_state(existing[reg], incoming[reg]);
        if merged.ty != existing[reg].ty {
            existing[reg] = merged;
            changed = true;
        }
    }
    changed
}

fn join_reg_state(lhs: RegState, rhs: RegState) -> RegState {
    if lhs.ty == rhs.ty {
        lhs
    } else if matches!(
        (lhs.ty, rhs.ty),
        (RegType::MapValuePtr, RegType::MapValueOrNull)
            | (RegType::MapValueOrNull, RegType::MapValuePtr)
    ) {
        RegState::map_value_or_null()
    } else if !lhs.is_init() || !rhs.is_init() {
        RegState::default()
    } else {
        RegState::scalar()
    }
}

fn verify_call(
    regs: &mut [RegState; BPF_MAX_REGS],
    helper_id: u32,
    insn_idx: usize,
    log: &mut VerifierLog,
) -> AxResult<()> {
    let Some(proto) = helper_proto(helper_id) else {
        log.log(&alloc::format!(
            "insn {insn_idx}: unknown helper function {helper_id}"
        ));
        return Err(AxError::InvalidInput);
    };

    for (arg_idx, arg_kind) in proto.args.iter().enumerate() {
        let reg = arg_idx + 1;
        match *arg_kind {
            HelperArgKind::Unused => {}
            HelperArgKind::Init => {
                check_reg_init(regs, reg, insn_idx, log)?;
            }
            HelperArgKind::Scalar => {
                check_reg_init(regs, reg, insn_idx, log)?;
                if regs[reg].ty != RegType::Scalar {
                    log.log(&alloc::format!(
                        "insn {insn_idx}: helper arg R{reg} must be scalar"
                    ));
                    return Err(AxError::InvalidInput);
                }
            }
            HelperArgKind::MapPtr => {
                check_reg_init(regs, reg, insn_idx, log)?;
                if regs[reg].ty != RegType::MapPtr {
                    log.log(&alloc::format!(
                        "insn {insn_idx}: helper arg R{reg} must be a map pointer"
                    ));
                    return Err(AxError::InvalidInput);
                }
            }
            HelperArgKind::Mem(mask) => {
                check_reg_init(regs, reg, insn_idx, log)?;
                if !mem_ptr_allowed(regs[reg].ty, mask) {
                    log.log(&alloc::format!(
                        "insn {insn_idx}: helper arg R{reg} has invalid memory pointer type"
                    ));
                    return Err(AxError::InvalidInput);
                }
            }
        }
    }

    clobber_caller_saved(regs);
    if proto.invalidates_map_value_ptrs {
        clobber_map_value_ptrs(regs);
    }
    regs[0] = match proto.ret {
        HelperReturnKind::Scalar => RegState::scalar(),
        HelperReturnKind::MapValuePtr => RegState::map_value(),
        HelperReturnKind::MapValueOrNull => RegState::map_value_or_null(),
    };
    Ok(())
}

#[derive(Clone, Copy)]
enum HelperArgKind {
    Unused,
    Init,
    Scalar,
    MapPtr,
    Mem(HelperMemMask),
}

#[derive(Clone, Copy)]
enum HelperReturnKind {
    Scalar,
    MapValuePtr,
    MapValueOrNull,
}

#[derive(Clone, Copy)]
struct HelperProto {
    args: [HelperArgKind; 5],
    ret: HelperReturnKind,
    invalidates_map_value_ptrs: bool,
}

fn helper_proto(helper_id: u32) -> Option<HelperProto> {
    let proto = match helper_id {
        BPF_FUNC_MAP_LOOKUP_ELEM => HelperProto {
            args: [
                HelperArgKind::MapPtr,
                HelperArgKind::Mem(HelperMemMask::READABLE),
                HelperArgKind::Unused,
                HelperArgKind::Unused,
                HelperArgKind::Unused,
            ],
            ret: HelperReturnKind::MapValueOrNull,
            invalidates_map_value_ptrs: false,
        },
        BPF_FUNC_MAP_UPDATE_ELEM => HelperProto {
            args: [
                HelperArgKind::MapPtr,
                HelperArgKind::Mem(HelperMemMask::READABLE),
                HelperArgKind::Mem(HelperMemMask::READABLE),
                HelperArgKind::Scalar,
                HelperArgKind::Unused,
            ],
            ret: HelperReturnKind::Scalar,
            invalidates_map_value_ptrs: true,
        },
        BPF_FUNC_MAP_DELETE_ELEM => HelperProto {
            args: [
                HelperArgKind::MapPtr,
                HelperArgKind::Mem(HelperMemMask::READABLE),
                HelperArgKind::Unused,
                HelperArgKind::Unused,
                HelperArgKind::Unused,
            ],
            ret: HelperReturnKind::Scalar,
            invalidates_map_value_ptrs: true,
        },
        BPF_FUNC_KTIME_GET_NS
        | BPF_FUNC_GET_PRANDOM_U32
        | BPF_FUNC_GET_SMP_PROCESSOR_ID
        | BPF_FUNC_GET_CURRENT_PID_TGID
        | BPF_FUNC_GET_CURRENT_UID_GID => HelperProto {
            args: [
                HelperArgKind::Unused,
                HelperArgKind::Unused,
                HelperArgKind::Unused,
                HelperArgKind::Unused,
                HelperArgKind::Unused,
            ],
            ret: HelperReturnKind::Scalar,
            invalidates_map_value_ptrs: false,
        },
        BPF_FUNC_GET_CURRENT_COMM => HelperProto {
            args: [
                HelperArgKind::Mem(HelperMemMask::WRITABLE),
                HelperArgKind::Scalar,
                HelperArgKind::Unused,
                HelperArgKind::Unused,
                HelperArgKind::Unused,
            ],
            ret: HelperReturnKind::Scalar,
            invalidates_map_value_ptrs: false,
        },
        BPF_FUNC_TRACE_PRINTK => HelperProto {
            args: [
                HelperArgKind::Mem(HelperMemMask::READABLE),
                HelperArgKind::Scalar,
                HelperArgKind::Init,
                HelperArgKind::Init,
                HelperArgKind::Init,
            ],
            ret: HelperReturnKind::Scalar,
            invalidates_map_value_ptrs: false,
        },
        _ => return None,
    };
    Some(proto)
}

fn mem_ptr_allowed(reg_ty: RegType, mask: HelperMemMask) -> bool {
    match reg_ty {
        RegType::StackPtr => mask.contains(HelperMemMask::STACK),
        RegType::CtxPtr => mask.contains(HelperMemMask::CTX),
        RegType::MapValuePtr => mask.contains(HelperMemMask::MAP_VALUE),
        _ => false,
    }
}

fn mem_ptr_writable(reg_ty: RegType) -> bool {
    !matches!(reg_ty, RegType::CtxPtr)
}

fn preserves_mem_ptr(insn: &BpfInsn, reg_ty: RegType) -> bool {
    insn.class() == BPF_CLASS_ALU64
        && (insn.code & BPF_SRC_X) == 0
        && matches!(insn.op(), BPF_OP_ADD | BPF_OP_SUB)
        && matches!(
            reg_ty,
            RegType::StackPtr | RegType::CtxPtr | RegType::MapValuePtr
        )
}

fn clobber_caller_saved(regs: &mut [RegState; BPF_MAX_REGS]) {
    for reg in 1..=5 {
        regs[reg] = RegState::default();
    }
}

fn clobber_map_value_ptrs(regs: &mut [RegState; BPF_MAX_REGS]) {
    for reg in regs.iter_mut() {
        if matches!(reg.ty, RegType::MapValuePtr | RegType::MapValueOrNull) {
            *reg = RegState::scalar();
        }
    }
}

fn is_ld_imm64_candidate(insn: &BpfInsn) -> bool {
    insn.class() == BPF_CLASS_LD
        && (insn.code & 0x18) == BPF_SIZE_DW
        && (insn.code & 0xe0) == BPF_MODE_IMM
}

fn validate_supported_opcode(
    insn: &BpfInsn,
    aux: BpfInsnAux,
    insn_idx: usize,
    log: &mut VerifierLog,
) -> AxResult<()> {
    match insn.class() {
        BPF_CLASS_ALU | BPF_CLASS_ALU64 => validate_alu_opcode(insn, insn_idx, log),
        BPF_CLASS_JMP | BPF_CLASS_JMP32 => validate_jmp_opcode(insn, insn_idx, log),
        BPF_CLASS_LDX => validate_ldx_opcode(insn, insn_idx, log),
        BPF_CLASS_ST => validate_st_opcode(insn, insn_idx, log),
        BPF_CLASS_STX => validate_stx_opcode(insn, insn_idx, log),
        BPF_CLASS_LD => {
            if !matches!(aux, BpfInsnAux::LdImm64Head(_)) {
                log.log(&alloc::format!(
                    "insn {insn_idx}: unsupported LD-class opcode {:#x}",
                    insn.code
                ));
                return Err(AxError::InvalidInput);
            }
            Ok(())
        }
        _ => {
            log.log(&alloc::format!(
                "insn {insn_idx}: unsupported instruction class {:#x}",
                insn.class()
            ));
            Err(AxError::InvalidInput)
        }
    }
}

fn validate_alu_opcode(insn: &BpfInsn, insn_idx: usize, log: &mut VerifierLog) -> AxResult<()> {
    match insn.op() {
        BPF_OP_ADD | BPF_OP_SUB | BPF_OP_MUL | BPF_OP_DIV | BPF_OP_OR | BPF_OP_AND | BPF_OP_LSH
        | BPF_OP_RSH | BPF_OP_NEG | BPF_OP_MOD | BPF_OP_XOR | BPF_OP_MOV | BPF_OP_ARSH => Ok(()),
        BPF_OP_END => {
            let valid_imm = match insn.class() {
                BPF_CLASS_ALU => matches!(insn.imm, 16 | 32),
                BPF_CLASS_ALU64 => matches!(insn.imm, 16 | 32 | 64),
                _ => false,
            };
            if !valid_imm {
                log.log(&alloc::format!(
                    "insn {insn_idx}: invalid END immediate {}",
                    insn.imm
                ));
                return Err(AxError::InvalidInput);
            }
            Ok(())
        }
        _ => {
            log.log(&alloc::format!(
                "insn {insn_idx}: unsupported ALU opcode {:#x}",
                insn.code
            ));
            Err(AxError::InvalidInput)
        }
    }
}

fn validate_jmp_opcode(insn: &BpfInsn, insn_idx: usize, log: &mut VerifierLog) -> AxResult<()> {
    let op = insn.op();
    let class = insn.class();
    let supported = matches!(
        op,
        BPF_OP_JA
            | BPF_OP_JEQ
            | BPF_OP_JGT
            | BPF_OP_JGE
            | BPF_OP_JSET
            | BPF_OP_JNE
            | BPF_OP_JSGT
            | BPF_OP_JSGE
            | BPF_OP_JLT
            | BPF_OP_JLE
            | BPF_OP_JSLT
            | BPF_OP_JSLE
    ) || (class == BPF_CLASS_JMP && matches!(op, BPF_OP_CALL | BPF_OP_EXIT));

    if supported {
        Ok(())
    } else {
        log.log(&alloc::format!(
            "insn {insn_idx}: unsupported JMP opcode {:#x}",
            insn.code
        ));
        Err(AxError::InvalidInput)
    }
}

fn validate_ldx_opcode(insn: &BpfInsn, insn_idx: usize, log: &mut VerifierLog) -> AxResult<()> {
    if (insn.code & 0xe0) != BPF_MODE_MEM || !is_basic_mem_size(insn.code & 0x18) {
        log.log(&alloc::format!(
            "insn {insn_idx}: unsupported LDX opcode {:#x}",
            insn.code
        ));
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn validate_st_opcode(insn: &BpfInsn, insn_idx: usize, log: &mut VerifierLog) -> AxResult<()> {
    if (insn.code & 0xe0) != BPF_MODE_MEM || !is_basic_mem_size(insn.code & 0x18) {
        log.log(&alloc::format!(
            "insn {insn_idx}: unsupported ST opcode {:#x}",
            insn.code
        ));
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn validate_stx_opcode(insn: &BpfInsn, insn_idx: usize, log: &mut VerifierLog) -> AxResult<()> {
    let mode = insn.code & 0xe0;
    let size = insn.code & 0x18;
    let valid = match mode {
        BPF_MODE_MEM => is_basic_mem_size(size),
        BPF_MODE_ATOMIC => {
            matches!(size, BPF_SIZE_W | BPF_SIZE_DW) && is_supported_atomic_op(insn.imm)
        }
        _ => false,
    };
    if !valid {
        log.log(&alloc::format!(
            "insn {insn_idx}: unsupported STX opcode {:#x}",
            insn.code
        ));
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn is_basic_mem_size(size: u8) -> bool {
    matches!(size, BPF_SIZE_B | BPF_SIZE_H | BPF_SIZE_W | BPF_SIZE_DW)
}

fn is_supported_atomic_op(op: i32) -> bool {
    let base = op & !BPF_ATOMIC_FETCH;
    matches!(
        base,
        BPF_ATOMIC_ADD | BPF_ATOMIC_OR | BPF_ATOMIC_AND | BPF_ATOMIC_XOR
    ) || matches!(op, BPF_ATOMIC_XCHG | BPF_ATOMIC_CMPXCHG)
}

fn verify_atomic(
    regs: &mut [RegState; BPF_MAX_REGS],
    insn: &BpfInsn,
    insn_idx: usize,
    log: &mut VerifierLog,
) -> AxResult<()> {
    let src = insn.src_reg() as usize;
    if regs[src].ty != RegType::Scalar {
        log.log(&alloc::format!("insn {insn_idx}: atomic src R{src} must be scalar"));
        return Err(AxError::InvalidInput);
    }

    if insn.imm == BPF_ATOMIC_CMPXCHG {
        check_reg_init(regs, 0, insn_idx, log)?;
        if regs[0].ty != RegType::Scalar {
            log.log(&alloc::format!(
                "insn {insn_idx}: CMPXCHG R0 must be scalar"
            ));
            return Err(AxError::InvalidInput);
        }
        regs[0] = RegState::scalar();
    }

    if (insn.imm & BPF_ATOMIC_FETCH) != 0 {
        regs[src] = RegState::scalar();
    }

    Ok(())
}

fn writes_dst_reg(insn: &BpfInsn, aux: BpfInsnAux) -> bool {
    match insn.class() {
        BPF_CLASS_ALU | BPF_CLASS_ALU64 | BPF_CLASS_LDX => true,
        BPF_CLASS_LD => matches!(aux, BpfInsnAux::LdImm64Head(_)),
        _ => false,
    }
}

fn writes_src_reg(insn: &BpfInsn) -> bool {
    insn.class() == BPF_CLASS_STX
        && (insn.code & 0xe0) == BPF_MODE_ATOMIC
        && (insn.imm & BPF_ATOMIC_FETCH) != 0
}

fn calc_jump_target(pc: usize, insn: &BpfInsn) -> i64 {
    pc as i64 + 1 + bpf_jump_delta(insn)
}

fn validate_jump_target(
    decoded: &[BpfInsnAux],
    target: i64,
    insn_idx: usize,
    log: &mut VerifierLog,
) -> AxResult<()> {
    if target < 0 || target >= decoded.len() as i64 {
        log.log(&alloc::format!(
            "insn {insn_idx}: jump target {target} out of bounds"
        ));
        return Err(AxError::InvalidInput);
    }

    if decoded[target as usize].is_continuation() {
        log.log(&alloc::format!(
            "insn {insn_idx}: jump target {target} lands in LD_IMM_DW continuation"
        ));
        return Err(AxError::InvalidInput);
    }

    Ok(())
}

fn insn_successors(insns: &[BpfInsn], decoded: &[BpfInsnAux], pc: usize) -> AxResult<Vec<usize>> {
    if decoded[pc].is_continuation() {
        return Err(AxError::InvalidInput);
    }

    let insn = &insns[pc];
    if matches!(decoded[pc], BpfInsnAux::LdImm64Head(_)) {
        return Ok(next_successor(pc + 2, insns.len()));
    }

    match insn.class() {
        BPF_CLASS_JMP | BPF_CLASS_JMP32 => {
            let op = insn.op();
            if op == BPF_OP_EXIT {
                Ok(Vec::new())
            } else if op == BPF_OP_CALL {
                Ok(next_successor(pc + 1, insns.len()))
            } else {
                let target = calc_jump_target(pc, insn) as usize;
                if op == BPF_OP_JA {
                    Ok(vec![target])
                } else {
                    let mut succs = next_successor(pc + 1, insns.len());
                    succs.push(target);
                    Ok(succs)
                }
            }
        }
        _ => Ok(next_successor(pc + 1, insns.len())),
    }
}

fn successor_states(
    insns: &[BpfInsn],
    decoded: &[BpfInsnAux],
    pc: usize,
    regs: &[RegState; BPF_MAX_REGS],
) -> AxResult<Vec<(usize, [RegState; BPF_MAX_REGS])>> {
    let insn = &insns[pc];
    let succs = insn_successors(insns, decoded, pc)?;

    if !matches!(insn.class(), BPF_CLASS_JMP) || insn.code & BPF_SRC_X != 0 || insn.imm != 0 {
        return Ok(succs.into_iter().map(|succ| (succ, *regs)).collect());
    }

    let dst = insn.dst_reg() as usize;
    if regs[dst].ty != RegType::MapValueOrNull {
        return Ok(succs.into_iter().map(|succ| (succ, *regs)).collect());
    }

    let target = calc_jump_target(pc, insn) as usize;
    let fallthrough = pc + 1;
    let mut null_regs = *regs;
    let mut nonnull_regs = *regs;
    nonnull_regs[dst] = RegState::map_value();

    match insn.op() {
        BPF_OP_JEQ => Ok(succs
            .into_iter()
            .map(|succ| {
                if succ == target {
                    (succ, null_regs)
                } else if succ == fallthrough {
                    (succ, nonnull_regs)
                } else {
                    (succ, *regs)
                }
            })
            .collect()),
        BPF_OP_JNE => Ok(succs
            .into_iter()
            .map(|succ| {
                if succ == target {
                    (succ, nonnull_regs)
                } else if succ == fallthrough {
                    (succ, null_regs)
                } else {
                    (succ, *regs)
                }
            })
            .collect()),
        _ => Ok(succs.into_iter().map(|succ| (succ, *regs)).collect()),
    }
}

fn next_successor(next: usize, len: usize) -> Vec<usize> {
    if next < len { vec![next] } else { Vec::new() }
}
