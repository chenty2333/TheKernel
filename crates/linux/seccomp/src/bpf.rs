use alloc::vec::Vec;

use crate::{BPF_MAXINSNS, SECCOMP_DATA_SIZE};

/// Linux `struct sock_filter`, the eight-byte classic-BPF instruction format.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct ClassicBpfInstruction {
    /// Encoded classic-BPF opcode.
    pub code: u16,
    /// Conditional true-branch offset.
    pub jt: u8,
    /// Conditional false-branch offset.
    pub jf: u8,
    /// Opcode-specific immediate value.
    pub k: u32,
}

impl ClassicBpfInstruction {
    /// Constructs an instruction from its Linux UAPI fields.
    pub const fn new(code: u16, jt: u8, jf: u8, k: u32) -> Self {
        Self { code, jt, jf, k }
    }
    /// Constructs a non-branching instruction.
    pub const fn statement(code: u16, k: u32) -> Self {
        Self::new(code, 0, 0, k)
    }
    /// Constructs a conditional branch instruction.
    pub const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Self {
        Self::new(code, jt, jf, k)
    }
}

/// Raw Linux classic-BPF opcode values.
#[allow(missing_docs)]
pub mod opcode {
    pub const LD_IMM: u16 = 0x00;
    pub const LD_W_ABS: u16 = 0x20;
    pub const LD_MEM: u16 = 0x60;
    pub const LD_LEN: u16 = 0x80;
    pub const LDX_IMM: u16 = 0x01;
    pub const LDX_MEM: u16 = 0x61;
    pub const LDX_LEN: u16 = 0x81;
    pub const ST: u16 = 0x02;
    pub const STX: u16 = 0x03;
    pub const ALU_ADD_K: u16 = 0x04;
    pub const ALU_ADD_X: u16 = 0x0c;
    pub const ALU_SUB_K: u16 = 0x14;
    pub const ALU_SUB_X: u16 = 0x1c;
    pub const ALU_MUL_K: u16 = 0x24;
    pub const ALU_MUL_X: u16 = 0x2c;
    pub const ALU_DIV_K: u16 = 0x34;
    pub const ALU_DIV_X: u16 = 0x3c;
    pub const ALU_OR_K: u16 = 0x44;
    pub const ALU_OR_X: u16 = 0x4c;
    pub const ALU_AND_K: u16 = 0x54;
    pub const ALU_AND_X: u16 = 0x5c;
    pub const ALU_LSH_K: u16 = 0x64;
    pub const ALU_LSH_X: u16 = 0x6c;
    pub const ALU_RSH_K: u16 = 0x74;
    pub const ALU_RSH_X: u16 = 0x7c;
    pub const ALU_NEG: u16 = 0x84;
    pub const ALU_XOR_K: u16 = 0xa4;
    pub const ALU_XOR_X: u16 = 0xac;
    pub const JMP_JA: u16 = 0x05;
    pub const JMP_JEQ_K: u16 = 0x15;
    pub const JMP_JEQ_X: u16 = 0x1d;
    pub const JMP_JGT_K: u16 = 0x25;
    pub const JMP_JGT_X: u16 = 0x2d;
    pub const JMP_JGE_K: u16 = 0x35;
    pub const JMP_JGE_X: u16 = 0x3d;
    pub const JMP_JSET_K: u16 = 0x45;
    pub const JMP_JSET_X: u16 = 0x4d;
    pub const RET_K: u16 = 0x06;
    pub const RET_A: u16 = 0x16;
    pub const MISC_TAX: u16 = 0x07;
    pub const MISC_TXA: u16 = 0x87;
}

/// Immutable syscall facts visible to a seccomp filter.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[repr(C)]
pub struct SeccompData {
    /// Original Linux syscall number.
    pub number: i32,
    /// Linux audit architecture value.
    pub architecture: u32,
    /// Userspace instruction pointer after the syscall instruction.
    pub instruction_pointer: u64,
    /// Six original syscall argument registers.
    pub arguments: [u64; 6],
}

impl SeccompData {
    /// Returns the exact 64-byte native-endian byte view consumed by a
    /// native seccomp translator.
    ///
    /// The returned object is intentionally owned: it keeps the input ABI
    /// explicit without exposing a reference created through an unsafe cast
    /// or relying on a packed representation. On x86_64 this is the native
    /// `struct seccomp_data` layout and is safe to pass to an executor for
    /// the duration of one evaluation.
    pub fn native_bytes(&self) -> [u8; SECCOMP_DATA_SIZE] {
        let mut bytes = [0u8; SECCOMP_DATA_SIZE];
        bytes[0..4].copy_from_slice(&self.number.to_ne_bytes());
        bytes[4..8].copy_from_slice(&self.architecture.to_ne_bytes());
        bytes[8..16].copy_from_slice(&self.instruction_pointer.to_ne_bytes());
        for (index, argument) in self.arguments.iter().copied().enumerate() {
            let start = 16 + index * 8;
            bytes[start..start + 8].copy_from_slice(&argument.to_ne_bytes());
        }
        bytes
    }
}

/// Narrow mechanism-neutral execution contract for one immutable seccomp plan.
///
/// Implementations are supplied by the embedding kernel and may retain a
/// generic classic-BPF interpreter and an optional native image. They must not
/// allocate, mutate policy state, or inspect the current task while executing.
pub trait SeccompExecutor: Send + Sync {
    /// Evaluates the 64-byte native seccomp-data view and returns a raw action.
    fn execute(&self, data: &[u8]) -> u32;
}

/// Rejection reason produced while validating an untrusted seccomp program.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ProgramError {
    /// The program contains no instructions.
    Empty,
    /// The program exceeds Linux's 4096-instruction limit.
    TooLong,
    /// Allocation for validation metadata or the immutable program failed.
    NoMemory,
    /// An opcode is not in Linux's seccomp cBPF subset.
    InvalidOpcode {
        /// Instruction index.
        program_counter: usize,
        /// Rejected encoded opcode.
        code: u16,
    },
    /// A seccomp-data load is not 32-bit aligned.
    DataOffsetUnaligned {
        /// Instruction index.
        program_counter: usize,
    },
    /// A seccomp-data load is outside the 64-byte input object.
    DataOffsetOutOfRange {
        /// Instruction index.
        program_counter: usize,
    },
}

impl ProgramError {
    /// Returns whether validation failed because an allocation was unavailable.
    pub const fn is_no_memory(self) -> bool {
        matches!(self, Self::NoMemory)
    }
}

/// Linux-specific profile for an immutable seccomp classic-BPF instruction stream.
///
/// This validates only constraints owned by the Linux seccomp ABI: its opcode
/// subset and the `seccomp_data` load layout. Generic classic-BPF validation
/// and execution deliberately belong to the mechanism selected by the kernel.
#[derive(Debug)]
pub struct VerifiedProgram {
    instructions: Vec<ClassicBpfInstruction>,
    path_charge: usize,
}

impl VerifiedProgram {
    /// Validates the Linux profile and takes ownership of a complete userspace copy.
    pub fn try_from_vec(instructions: Vec<ClassicBpfInstruction>) -> Result<Self, ProgramError> {
        verify_seccomp_profile(&instructions)?;
        let path_charge = linux_v6_12_unblinded_migration_charge(&instructions);
        Ok(Self {
            instructions,
            path_charge,
        })
    }

    /// Fallibly copies and validates the Linux profile.
    pub fn try_copy_from_slice(
        instructions: &[ClassicBpfInstruction],
    ) -> Result<Self, ProgramError> {
        verify_seccomp_profile(instructions)?;
        let path_charge = linux_v6_12_unblinded_migration_charge(instructions);
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(instructions.len())
            .map_err(|_| ProgramError::NoMemory)?;
        owned.extend_from_slice(instructions);
        Ok(Self {
            instructions: owned,
            path_charge,
        })
    }

    /// Returns the classic-BPF instruction count.
    pub fn len(&self) -> usize {
        self.instructions.len()
    }

    /// Returns whether this profile is empty. Valid profiles are never empty.
    pub fn is_empty(&self) -> bool {
        self.instructions.is_empty()
    }

    /// Returns the Linux v6.12 unblinded cBPF-to-eBPF migration charge used
    /// for stacked seccomp path accounting.
    ///
    /// Linux accounts the converted `bpf_prog::len`, not the userspace cBPF
    /// length. TheKernel has no eBPF JIT or constant blinding, so this stable
    /// baseline deliberately excludes architecture-specific JIT hardening.
    pub fn path_charge(&self) -> usize {
        self.path_charge
    }

    /// Returns the immutable Linux-profile instruction sequence.
    pub fn instructions(&self) -> &[ClassicBpfInstruction] {
        &self.instructions
    }
}

const LINUX_V6_12_CLASSIC_MIGRATION_PROLOGUE: usize = 3;

fn linux_v6_12_unblinded_migration_charge(instructions: &[ClassicBpfInstruction]) -> usize {
    LINUX_V6_12_CLASSIC_MIGRATION_PROLOGUE
        + instructions
            .iter()
            .map(linux_v6_12_unblinded_instruction_charge)
            .sum::<usize>()
}

fn linux_v6_12_unblinded_instruction_charge(instruction: &ClassicBpfInstruction) -> usize {
    match instruction.code {
        // Linux guards register division by zero before the converted ALU
        // instruction: normalize X, branch, zero A, exit, then divide.
        opcode::ALU_DIV_X => 5,
        // A constant return becomes MOV32 plus EXIT.
        opcode::RET_K => 2,
        opcode::JMP_JEQ_K | opcode::JMP_JGE_K | opcode::JMP_JGT_K | opcode::JMP_JSET_K => {
            usize::from((instruction.k as i32) < 0) + converted_conditional_jump_charge(instruction)
        }
        opcode::JMP_JEQ_X | opcode::JMP_JGE_X | opcode::JMP_JGT_X | opcode::JMP_JSET_X => {
            converted_conditional_jump_charge(instruction)
        }
        _ => 1,
    }
}

fn converted_conditional_jump_charge(instruction: &ClassicBpfInstruction) -> usize {
    if instruction.jf == 0
        || (instruction.jt == 0
            && !matches!(instruction.code, opcode::JMP_JSET_K | opcode::JMP_JSET_X))
    {
        1
    } else {
        // A conditional branch with two non-fallthrough targets becomes the
        // conditional jump followed by an unconditional JA. JSET has no
        // inverse form, so it also needs JA when only the true side falls
        // through.
        2
    }
}

fn verify_seccomp_profile(instructions: &[ClassicBpfInstruction]) -> Result<(), ProgramError> {
    if instructions.is_empty() {
        return Err(ProgramError::Empty);
    }
    if instructions.len() > BPF_MAXINSNS {
        return Err(ProgramError::TooLong);
    }
    for (program_counter, instruction) in instructions.iter().enumerate() {
        if !allowed_seccomp_opcode(instruction.code) {
            return Err(ProgramError::InvalidOpcode {
                program_counter,
                code: instruction.code,
            });
        }
        if instruction.code == opcode::LD_W_ABS {
            if instruction.k & 3 != 0 {
                return Err(ProgramError::DataOffsetUnaligned { program_counter });
            }
            if instruction.k as usize >= SECCOMP_DATA_SIZE {
                return Err(ProgramError::DataOffsetOutOfRange { program_counter });
            }
        }
    }
    Ok(())
}

const fn allowed_seccomp_opcode(code: u16) -> bool {
    matches!(
        code,
        opcode::LD_W_ABS
            | opcode::LD_LEN
            | opcode::LDX_LEN
            | opcode::RET_K
            | opcode::RET_A
            | opcode::ALU_ADD_K
            | opcode::ALU_ADD_X
            | opcode::ALU_SUB_K
            | opcode::ALU_SUB_X
            | opcode::ALU_MUL_K
            | opcode::ALU_MUL_X
            | opcode::ALU_DIV_K
            | opcode::ALU_DIV_X
            | opcode::ALU_AND_K
            | opcode::ALU_AND_X
            | opcode::ALU_OR_K
            | opcode::ALU_OR_X
            | opcode::ALU_XOR_K
            | opcode::ALU_XOR_X
            | opcode::ALU_LSH_K
            | opcode::ALU_LSH_X
            | opcode::ALU_RSH_K
            | opcode::ALU_RSH_X
            | opcode::ALU_NEG
            | opcode::LD_IMM
            | opcode::LDX_IMM
            | opcode::MISC_TAX
            | opcode::MISC_TXA
            | opcode::LD_MEM
            | opcode::LDX_MEM
            | opcode::ST
            | opcode::STX
            | opcode::JMP_JA
            | opcode::JMP_JEQ_K
            | opcode::JMP_JEQ_X
            | opcode::JMP_JGE_K
            | opcode::JMP_JGE_X
            | opcode::JMP_JGT_K
            | opcode::JMP_JGT_X
            | opcode::JMP_JSET_K
            | opcode::JMP_JSET_X
    )
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::mem::{align_of, offset_of, size_of};

    use super::*;
    use crate::{AUDIT_ARCH_X86_64, SECCOMP_RET_ALLOW, SECCOMP_RET_ERRNO};

    const fn stmt(code: u16, value: u32) -> ClassicBpfInstruction {
        ClassicBpfInstruction::statement(code, value)
    }

    const fn jump(code: u16, value: u32, jt: u8, jf: u8) -> ClassicBpfInstruction {
        ClassicBpfInstruction::jump(code, value, jt, jf)
    }

    fn data() -> SeccompData {
        SeccompData {
            number: 63,
            architecture: AUDIT_ARCH_X86_64,
            instruction_pointer: 0x1122_3344_5566_7788,
            arguments: [0x0102_0304_0506_0708, 2, 3, 4, 5, 0xa1a2_a3a4_a5a6_a7a8],
        }
    }

    #[test]
    fn seccomp_data_and_audit_arch_matches_linux_x86_64_abi() {
        assert_eq!(size_of::<SeccompData>(), 64);
        assert_eq!(align_of::<SeccompData>(), 8);
        assert_eq!(offset_of!(SeccompData, number), 0);
        assert_eq!(offset_of!(SeccompData, architecture), 4);
        assert_eq!(offset_of!(SeccompData, instruction_pointer), 8);
        assert_eq!(offset_of!(SeccompData, arguments), 16);
        assert_eq!(AUDIT_ARCH_X86_64, 0xc000_003e);
    }

    #[test]
    fn native_bytes_preserve_the_wire_layout_without_aliasing() {
        let bytes = data().native_bytes();
        assert_eq!(&bytes[0..4], &63i32.to_ne_bytes());
        assert_eq!(&bytes[4..8], &AUDIT_ARCH_X86_64.to_ne_bytes());
        assert_eq!(&bytes[8..16], &0x1122_3344_5566_7788u64.to_ne_bytes());
        assert_eq!(&bytes[16..24], &0x0102_0304_0506_0708u64.to_ne_bytes());
        assert_eq!(bytes.len(), SECCOMP_DATA_SIZE);
    }

    #[test]
    fn rejects_empty_and_oversize_linux_profiles() {
        assert!(matches!(
            VerifiedProgram::try_copy_from_slice(&[]),
            Err(ProgramError::Empty)
        ));
        let mut huge = Vec::new();
        huge.try_reserve_exact(BPF_MAXINSNS + 1).unwrap();
        huge.resize(BPF_MAXINSNS + 1, stmt(opcode::RET_K, 0));
        assert!(matches!(
            VerifiedProgram::try_from_vec(huge),
            Err(ProgramError::TooLong)
        ));
    }

    #[test]
    fn rejects_non_seccomp_loads_modulo_and_bad_offsets() {
        for code in [
            0x28, // LD_H_ABS
            0x40, // LD_W_IND
            0xb1, // LDX_B_MSH
            0x94, // ALU_MOD_K
        ] {
            assert!(matches!(
                VerifiedProgram::try_copy_from_slice(&[stmt(code, 0), stmt(opcode::RET_A, 0),]),
                Err(ProgramError::InvalidOpcode { .. })
            ));
        }
        assert_eq!(
            VerifiedProgram::try_copy_from_slice(&[
                stmt(opcode::LD_W_ABS, 2),
                stmt(opcode::RET_A, 0),
            ])
            .unwrap_err(),
            ProgramError::DataOffsetUnaligned { program_counter: 0 }
        );
        assert_eq!(
            VerifiedProgram::try_copy_from_slice(&[
                stmt(opcode::LD_W_ABS, 64),
                stmt(opcode::RET_A, 0),
            ])
            .unwrap_err(),
            ProgramError::DataOffsetOutOfRange { program_counter: 0 }
        );
    }

    #[test]
    fn leaves_generic_classic_bpf_validation_to_the_mechanism() {
        let profile = VerifiedProgram::try_copy_from_slice(&[
            stmt(opcode::ALU_DIV_K, 0),
            stmt(opcode::RET_A, 0),
        ])
        .unwrap();
        assert_eq!(profile.instructions()[0].code, opcode::ALU_DIV_K);
    }

    #[test]
    fn path_charge_matches_linux_v6_12_ret_k_and_div_x_expansion() {
        let ret_k = VerifiedProgram::try_copy_from_slice(&[stmt(opcode::RET_K, SECCOMP_RET_ALLOW)])
            .unwrap();
        assert_eq!(ret_k.len(), 1);
        assert_eq!(ret_k.path_charge(), 5); // Three-prologue + MOV32 + EXIT.

        let mut maximum_ret_k = Vec::new();
        maximum_ret_k.try_reserve_exact(BPF_MAXINSNS).unwrap();
        maximum_ret_k.resize(BPF_MAXINSNS, stmt(opcode::RET_K, SECCOMP_RET_ALLOW));
        let maximum_ret_k = VerifiedProgram::try_from_vec(maximum_ret_k).unwrap();
        assert_eq!(maximum_ret_k.len(), BPF_MAXINSNS);
        assert_eq!(maximum_ret_k.path_charge(), 3 + 2 * BPF_MAXINSNS);

        let ret_a = VerifiedProgram::try_copy_from_slice(&[stmt(opcode::RET_A, 0)]).unwrap();
        assert_eq!(ret_a.path_charge(), 4); // Three-prologue + EXIT.

        let div_x = VerifiedProgram::try_copy_from_slice(&[
            stmt(opcode::ALU_DIV_X, 0),
            stmt(opcode::RET_A, 0),
        ])
        .unwrap();
        assert_eq!(div_x.len(), 2);
        assert_eq!(div_x.path_charge(), 9); // Three-prologue + five + EXIT.
    }

    #[test]
    fn path_charge_distinguishes_reversible_and_non_reversible_conditionals() {
        let reversible = VerifiedProgram::try_copy_from_slice(&[
            jump(opcode::JMP_JEQ_K, 7, 0, 1),
            stmt(opcode::RET_K, SECCOMP_RET_ALLOW),
            stmt(opcode::RET_K, SECCOMP_RET_ERRNO),
        ])
        .unwrap();
        assert_eq!(reversible.path_charge(), 8);

        // Linux can invert JEQ when the true target falls through, but JSET
        // has no inverse opcode and therefore also needs an unconditional JA.
        let non_reversible = VerifiedProgram::try_copy_from_slice(&[
            jump(opcode::JMP_JSET_K, 7, 0, 1),
            stmt(opcode::RET_K, SECCOMP_RET_ALLOW),
            stmt(opcode::RET_K, SECCOMP_RET_ERRNO),
        ])
        .unwrap();
        assert_eq!(non_reversible.path_charge(), 9);

        // A negative K compare first materializes the unsigned immediate in
        // a temporary eBPF register, adding one more converted instruction.
        let negative_immediate = VerifiedProgram::try_copy_from_slice(&[
            jump(opcode::JMP_JEQ_K, u32::MAX, 0, 1),
            stmt(opcode::RET_K, SECCOMP_RET_ALLOW),
            stmt(opcode::RET_K, SECCOMP_RET_ERRNO),
        ])
        .unwrap();
        assert_eq!(negative_immediate.path_charge(), 9);
    }
}
