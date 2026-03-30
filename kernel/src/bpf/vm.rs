//! eBPF interpreter (virtual machine).
//!
//! Executes verified BPF bytecode with a register-based fetch-decode-execute
//! loop. Implements the full eBPF ISA including ALU32/ALU64, JMP/JMP32,
//! LDX/STX/ST, and 64-bit immediate loads.

use alloc::{sync::Arc, vec, vec::Vec};

use axerrno::{AxError, AxResult};

use super::{
    defs::*,
    helpers::{self, HelperContext, MapValueRegion},
    map::BpfMap,
};

/// eBPF virtual machine state.
pub struct BpfVm<'a> {
    regs: [u64; BPF_MAX_REGS],
    stack: [u8; BPF_STACK_SIZE],
    insns: &'a [BpfInsn],
    decoded_insns: &'a [BpfInsnAux],
    pc: usize,
    maps: &'a [Arc<dyn BpfMap>],
    /// Stable regions backing pointers returned by map lookup helpers.
    map_value_regions: Vec<MapValueRegion>,
    /// Context buffer base address and size.
    ctx_base: u64,
    ctx_size: usize,
    /// Remaining auxiliary copy/allocation budget for this execution.
    aux_budget_remaining: u64,
}

impl<'a> BpfVm<'a> {
    pub fn new(
        insns: &'a [BpfInsn],
        decoded_insns: &'a [BpfInsnAux],
        maps: &'a [Arc<dyn BpfMap>],
    ) -> Self {
        Self {
            regs: [0; BPF_MAX_REGS],
            stack: [0; BPF_STACK_SIZE],
            insns,
            decoded_insns,
            pc: 0,
            maps,
            map_value_regions: Vec::new(),
            ctx_base: 0,
            ctx_size: 0,
            aux_budget_remaining: u64::MAX,
        }
    }

    pub fn with_aux_budget(
        insns: &'a [BpfInsn],
        decoded_insns: &'a [BpfInsnAux],
        maps: &'a [Arc<dyn BpfMap>],
        aux_budget_remaining: u64,
    ) -> Self {
        let mut vm = Self::new(insns, decoded_insns, maps);
        vm.aux_budget_remaining = aux_budget_remaining;
        vm
    }

    pub fn remaining_aux_budget(&self) -> u64 {
        self.aux_budget_remaining
    }

    /// Execute the BPF program with the given context buffer.
    ///
    /// Returns the value in R0 (the program's return value).
    pub fn execute(&mut self, ctx: &mut [u8]) -> AxResult<u64> {
        // Set up initial state
        self.regs = [0; BPF_MAX_REGS];
        self.pc = 0;
        self.map_value_regions.clear();

        let stack_top = self.stack.as_ptr() as u64 + BPF_STACK_SIZE as u64;
        self.regs[1] = ctx.as_ptr() as u64; // R1 = context pointer
        self.regs[BPF_REG_FP] = stack_top; // R10 = frame pointer (top of stack)

        self.ctx_base = ctx.as_ptr() as u64;
        self.ctx_size = ctx.len();

        let mut insn_count: usize = 0;

        loop {
            if insn_count >= BPF_MAX_EXEC_INSNS {
                return Err(AxError::ResourceBusy);
            }
            if self.pc >= self.insns.len() {
                return Err(AxError::InvalidInput);
            }
            if self.decoded_insns[self.pc].is_continuation() {
                return Err(AxError::InvalidInput);
            }

            let insn = self.insns[self.pc];
            insn_count += 1;

            let class = insn.code & 0x07;
            match class {
                BPF_CLASS_ALU64 => self.exec_alu64(&insn)?,
                BPF_CLASS_ALU => self.exec_alu32(&insn)?,
                BPF_CLASS_JMP => {
                    if let Some(ret) = self.exec_jmp(&insn)? {
                        return Ok(ret);
                    }
                }
                BPF_CLASS_JMP32 => {
                    self.exec_jmp32(&insn)?;
                }
                BPF_CLASS_LDX => self.exec_ldx(&insn)?,
                BPF_CLASS_STX => self.exec_stx(&insn)?,
                BPF_CLASS_ST => self.exec_st(&insn)?,
                BPF_CLASS_LD => self.exec_ld(&insn)?,
                _ => return Err(AxError::InvalidInput),
            }
        }
    }

    // -----------------------------------------------------------------------
    // ALU64 (64-bit arithmetic)
    // -----------------------------------------------------------------------

    fn exec_alu64(&mut self, insn: &BpfInsn) -> AxResult<()> {
        let dst = insn.dst_reg() as usize;
        let src_val = self.src_value(insn);
        let op = insn.code & 0xf0;

        self.regs[dst] = match op {
            BPF_OP_ADD => self.regs[dst].wrapping_add(src_val),
            BPF_OP_SUB => self.regs[dst].wrapping_sub(src_val),
            BPF_OP_MUL => self.regs[dst].wrapping_mul(src_val),
            BPF_OP_DIV => {
                if src_val == 0 {
                    0
                } else {
                    self.regs[dst] / src_val
                }
            }
            BPF_OP_MOD => {
                if src_val == 0 {
                    self.regs[dst]
                } else {
                    self.regs[dst] % src_val
                }
            }
            BPF_OP_OR => self.regs[dst] | src_val,
            BPF_OP_AND => self.regs[dst] & src_val,
            BPF_OP_LSH => self.regs[dst] << (src_val & 63),
            BPF_OP_RSH => self.regs[dst] >> (src_val & 63),
            BPF_OP_NEG => (-(self.regs[dst] as i64)) as u64,
            BPF_OP_XOR => self.regs[dst] ^ src_val,
            BPF_OP_MOV => src_val,
            BPF_OP_ARSH => ((self.regs[dst] as i64) >> (src_val & 63)) as u64,
            BPF_OP_END => {
                // Byte swap
                match insn.imm {
                    16 => {
                        if insn.code & BPF_SRC_X != 0 {
                            (self.regs[dst] as u16).to_be() as u64
                        } else {
                            (self.regs[dst] as u16).to_le() as u64
                        }
                    }
                    32 => {
                        if insn.code & BPF_SRC_X != 0 {
                            (self.regs[dst] as u32).to_be() as u64
                        } else {
                            (self.regs[dst] as u32).to_le() as u64
                        }
                    }
                    64 => {
                        if insn.code & BPF_SRC_X != 0 {
                            self.regs[dst].to_be()
                        } else {
                            self.regs[dst].to_le()
                        }
                    }
                    _ => return Err(AxError::InvalidInput),
                }
            }
            _ => return Err(AxError::InvalidInput),
        };
        self.pc += 1;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // ALU32 (32-bit arithmetic, result zero-extended to 64 bits)
    // -----------------------------------------------------------------------

    fn exec_alu32(&mut self, insn: &BpfInsn) -> AxResult<()> {
        let dst = insn.dst_reg() as usize;
        let dst_val = self.regs[dst] as u32;
        let src_val = self.src_value(insn) as u32;
        let op = insn.code & 0xf0;

        let result: u32 = match op {
            BPF_OP_ADD => dst_val.wrapping_add(src_val),
            BPF_OP_SUB => dst_val.wrapping_sub(src_val),
            BPF_OP_MUL => dst_val.wrapping_mul(src_val),
            BPF_OP_DIV => {
                if src_val == 0 {
                    0
                } else {
                    dst_val / src_val
                }
            }
            BPF_OP_MOD => {
                if src_val == 0 {
                    dst_val
                } else {
                    dst_val % src_val
                }
            }
            BPF_OP_OR => dst_val | src_val,
            BPF_OP_AND => dst_val & src_val,
            BPF_OP_LSH => dst_val << (src_val & 31),
            BPF_OP_RSH => dst_val >> (src_val & 31),
            BPF_OP_NEG => (-(dst_val as i32)) as u32,
            BPF_OP_XOR => dst_val ^ src_val,
            BPF_OP_MOV => src_val,
            BPF_OP_ARSH => ((dst_val as i32) >> (src_val & 31)) as u32,
            BPF_OP_END => match insn.imm {
                16 => {
                    if insn.code & BPF_SRC_X != 0 {
                        (dst_val as u16).to_be() as u32
                    } else {
                        (dst_val as u16).to_le() as u32
                    }
                }
                32 => {
                    if insn.code & BPF_SRC_X != 0 {
                        dst_val.to_be()
                    } else {
                        dst_val.to_le()
                    }
                }
                _ => return Err(AxError::InvalidInput),
            },
            _ => return Err(AxError::InvalidInput),
        };

        // Zero-extend 32-bit result to 64-bit
        self.regs[dst] = result as u64;
        self.pc += 1;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // JMP (64-bit comparisons, CALL, EXIT)
    // -----------------------------------------------------------------------

    /// Returns `Some(retval)` on EXIT, `None` otherwise.
    fn exec_jmp(&mut self, insn: &BpfInsn) -> AxResult<Option<u64>> {
        let dst = insn.dst_reg() as usize;
        let op = insn.code & 0xf0;

        if op == BPF_OP_EXIT {
            return Ok(Some(self.regs[0]));
        }

        if op == BPF_OP_CALL {
            let helper_id = insn.imm as u32;
            let mut hctx = HelperContext {
                maps: self.maps,
                map_value_regions: &mut self.map_value_regions,
                stack: &mut self.stack,
                ctx_base: self.ctx_base,
                ctx_size: self.ctx_size,
                aux_budget_remaining: &mut self.aux_budget_remaining,
            };
            self.regs[0] = helpers::call_helper(
                helper_id,
                self.regs[1],
                self.regs[2],
                self.regs[3],
                self.regs[4],
                self.regs[5],
                &mut hctx,
            )?;
            self.pc += 1;
            return Ok(None);
        }

        if op == BPF_OP_JA {
            self.pc = ((self.pc as i64) + 1 + bpf_jump_delta(insn)) as usize;
            return Ok(None);
        }

        // Conditional jumps: 64-bit comparison
        let src_val = self.src_value(insn);
        let dst_val = self.regs[dst];
        let taken = eval_jmp_cond(op, dst_val, src_val);

        if taken {
            self.pc = ((self.pc as i64) + 1 + bpf_jump_delta(insn)) as usize;
        } else {
            self.pc += 1;
        }
        Ok(None)
    }

    // -----------------------------------------------------------------------
    // JMP32 (32-bit comparisons)
    // -----------------------------------------------------------------------

    fn exec_jmp32(&mut self, insn: &BpfInsn) -> AxResult<()> {
        let dst = insn.dst_reg() as usize;
        let op = insn.code & 0xf0;

        if op == BPF_OP_CALL || op == BPF_OP_EXIT {
            return Err(AxError::InvalidInput);
        }

        if op == BPF_OP_JA {
            self.pc = ((self.pc as i64) + 1 + bpf_jump_delta(insn)) as usize;
            return Ok(());
        }

        let src_val = self.src_value(insn) as u32 as u64;
        let dst_val = self.regs[dst] as u32 as u64;
        let taken = eval_jmp_cond(op, dst_val, src_val);

        if taken {
            self.pc = ((self.pc as i64) + 1 + bpf_jump_delta(insn)) as usize;
        } else {
            self.pc += 1;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // LDX (load from memory)
    // -----------------------------------------------------------------------

    fn exec_ldx(&mut self, insn: &BpfInsn) -> AxResult<()> {
        let dst = insn.dst_reg() as usize;
        let src = insn.src_reg() as usize;
        let addr = (self.regs[src] as i64 + insn.off as i64) as u64;
        let size = insn.code & 0x18;

        self.regs[dst] = match size {
            BPF_SIZE_B => self.mem_read::<u8>(addr)? as u64,
            BPF_SIZE_H => self.mem_read::<u16>(addr)? as u64,
            BPF_SIZE_W => self.mem_read::<u32>(addr)? as u64,
            BPF_SIZE_DW => self.mem_read::<u64>(addr)?,
            _ => return Err(AxError::InvalidInput),
        };

        self.pc += 1;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // STX (store register to memory)
    // -----------------------------------------------------------------------

    fn exec_stx(&mut self, insn: &BpfInsn) -> AxResult<()> {
        let dst = insn.dst_reg() as usize;
        let src = insn.src_reg() as usize;
        let addr = (self.regs[dst] as i64 + insn.off as i64) as u64;
        let size = insn.code & 0x18;
        let mode = insn.code & 0xe0;

        if mode == BPF_MODE_ATOMIC {
            return self.exec_atomic(insn, addr);
        }

        match size {
            BPF_SIZE_B => self.mem_write::<u8>(addr, self.regs[src] as u8)?,
            BPF_SIZE_H => self.mem_write::<u16>(addr, self.regs[src] as u16)?,
            BPF_SIZE_W => self.mem_write::<u32>(addr, self.regs[src] as u32)?,
            BPF_SIZE_DW => self.mem_write::<u64>(addr, self.regs[src])?,
            _ => return Err(AxError::InvalidInput),
        }

        self.pc += 1;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // ST (store immediate to memory)
    // -----------------------------------------------------------------------

    fn exec_st(&mut self, insn: &BpfInsn) -> AxResult<()> {
        let dst = insn.dst_reg() as usize;
        let addr = (self.regs[dst] as i64 + insn.off as i64) as u64;
        let size = insn.code & 0x18;

        // Sign-extend the 32-bit immediate to 64-bit
        let imm = insn.imm as i32 as i64 as u64;

        match size {
            BPF_SIZE_B => self.mem_write::<u8>(addr, imm as u8)?,
            BPF_SIZE_H => self.mem_write::<u16>(addr, imm as u16)?,
            BPF_SIZE_W => self.mem_write::<u32>(addr, imm as u32)?,
            BPF_SIZE_DW => self.mem_write::<u64>(addr, imm)?,
            _ => return Err(AxError::InvalidInput),
        }

        self.pc += 1;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // LD (64-bit immediate load, two-slot instruction)
    // -----------------------------------------------------------------------

    fn exec_ld(&mut self, insn: &BpfInsn) -> AxResult<()> {
        let dst = insn.dst_reg() as usize;

        self.regs[dst] = match self.decoded_insns[self.pc] {
            BpfInsnAux::LdImm64Head(BpfLdImm64Data::Immediate(imm64)) => imm64,
            BpfInsnAux::LdImm64Head(BpfLdImm64Data::MapIndex(map_index)) => map_index as u64,
            _ => return Err(AxError::InvalidInput),
        };
        self.pc += 2;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Atomic operations (STX with ATOMIC mode)
    // -----------------------------------------------------------------------

    fn exec_atomic(&mut self, insn: &BpfInsn, addr: u64) -> AxResult<()> {
        let src = insn.src_reg() as usize;
        let size = insn.code & 0x18;
        let atomic_op = insn.imm;
        let fetch = (atomic_op & BPF_ATOMIC_FETCH) != 0;
        let base_op = atomic_op & !BPF_ATOMIC_FETCH;

        // For simplicity, implement 64-bit atomics on the stack.
        // (In a unikernel without true SMP BPF contexts, this is sufficient.)
        match size {
            BPF_SIZE_DW => {
                let old_val = self.mem_read::<u64>(addr)?;
                let src_val = self.regs[src];
                let new_val = match base_op {
                    BPF_ATOMIC_ADD => old_val.wrapping_add(src_val),
                    BPF_ATOMIC_OR => old_val | src_val,
                    BPF_ATOMIC_AND => old_val & src_val,
                    BPF_ATOMIC_XOR => old_val ^ src_val,
                    _ if atomic_op == BPF_ATOMIC_XCHG => src_val,
                    _ if atomic_op == BPF_ATOMIC_CMPXCHG => {
                        if old_val == self.regs[0] {
                            src_val
                        } else {
                            old_val
                        }
                    }
                    _ => return Err(AxError::InvalidInput),
                };
                self.mem_write::<u64>(addr, new_val)?;
                if fetch {
                    self.regs[src] = old_val;
                }
                if atomic_op == BPF_ATOMIC_CMPXCHG {
                    self.regs[0] = old_val;
                }
            }
            BPF_SIZE_W => {
                let old_val = self.mem_read::<u32>(addr)?;
                let src_val = self.regs[src] as u32;
                let new_val = match base_op {
                    BPF_ATOMIC_ADD => old_val.wrapping_add(src_val),
                    BPF_ATOMIC_OR => old_val | src_val,
                    BPF_ATOMIC_AND => old_val & src_val,
                    BPF_ATOMIC_XOR => old_val ^ src_val,
                    _ if atomic_op == BPF_ATOMIC_XCHG => src_val,
                    _ if atomic_op == BPF_ATOMIC_CMPXCHG => {
                        if old_val == self.regs[0] as u32 {
                            src_val
                        } else {
                            old_val
                        }
                    }
                    _ => return Err(AxError::InvalidInput),
                };
                self.mem_write::<u32>(addr, new_val)?;
                if fetch {
                    self.regs[src] = old_val as u64;
                }
                if atomic_op == BPF_ATOMIC_CMPXCHG {
                    self.regs[0] = old_val as u64;
                }
            }
            _ => return Err(AxError::InvalidInput),
        }

        self.pc += 1;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Memory access helpers
    // -----------------------------------------------------------------------

    fn mem_read<T: Copy>(&self, addr: u64) -> AxResult<T> {
        let size = core::mem::size_of::<T>();
        let ptr = addr as usize;

        // Check stack region
        let stack_base = self.stack.as_ptr() as usize;
        if let Some(range) = helpers::checked_region(ptr, size, stack_base, BPF_STACK_SIZE) {
            let offset = range.start;
            return Ok(unsafe {
                core::ptr::read_unaligned(self.stack.as_ptr().add(offset) as *const T)
            });
        }

        // Check context region
        let ctx_base = self.ctx_base as usize;
        if helpers::checked_region(ptr, size, ctx_base, self.ctx_size).is_some() {
            return Ok(unsafe { core::ptr::read_unaligned(ptr as *const T) });
        }

        // Check helper-managed map value regions.
        for region in &self.map_value_regions {
            if let Some(bytes) = region.read_bytes(ptr, size) {
                helpers::charge_aux_budget(&mut self.aux_budget_remaining, bytes.len())?;
                return Ok(unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const T) });
            }
        }

        Err(AxError::BadAddress)
    }

    fn mem_write<T: Copy>(&mut self, addr: u64, val: T) -> AxResult<()> {
        let size = core::mem::size_of::<T>();
        let ptr = addr as usize;

        // Check stack region
        let stack_base = self.stack.as_ptr() as usize;
        if let Some(range) = helpers::checked_region(ptr, size, stack_base, BPF_STACK_SIZE) {
            let offset = range.start;
            unsafe {
                core::ptr::write_unaligned(self.stack.as_mut_ptr().add(offset) as *mut T, val);
            }
            return Ok(());
        }

        let bytes = unsafe { core::slice::from_raw_parts((&val as *const T) as *const u8, size) };
        for region in &mut self.map_value_regions {
            if region.contains_range(ptr, size) {
                region.write_bytes(ptr, bytes, &mut self.aux_budget_remaining)?;
                return Ok(());
            }
        }

        // Context is typically read-only for most program types, but some
        // program types allow writing. For Phase 1, disallow context writes.
        Err(AxError::BadAddress)
    }

    // -----------------------------------------------------------------------
    // Utilities
    // -----------------------------------------------------------------------

    /// Get the source operand value: either src register or sign-extended imm.
    fn src_value(&self, insn: &BpfInsn) -> u64 {
        if insn.code & BPF_SRC_X != 0 {
            self.regs[insn.src_reg() as usize]
        } else {
            insn.imm as i32 as i64 as u64
        }
    }
}

// ---------------------------------------------------------------------------
// Jump condition evaluator
// ---------------------------------------------------------------------------

fn eval_jmp_cond(op: u8, dst: u64, src: u64) -> bool {
    match op {
        BPF_OP_JEQ => dst == src,
        BPF_OP_JNE => dst != src,
        BPF_OP_JGT => dst > src,
        BPF_OP_JGE => dst >= src,
        BPF_OP_JLT => dst < src,
        BPF_OP_JLE => dst <= src,
        BPF_OP_JSGT => (dst as i64) > (src as i64),
        BPF_OP_JSGE => (dst as i64) >= (src as i64),
        BPF_OP_JSLT => (dst as i64) < (src as i64),
        BPF_OP_JSLE => (dst as i64) <= (src as i64),
        BPF_OP_JSET => (dst & src) != 0,
        _ => false,
    }
}
