#![doc = include_str!("../README.md")]
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

mod instruction;
mod program;
mod translate;
mod translation_validate;

pub use instruction::{Instruction, opcode};
pub use program::{Input, LoadWidth, MAX_INSTRUCTIONS, Program, SCRATCH_WORDS, VerifyError};
pub use translate::{
    CodeImage, ExternalCall, ImageValidationError, InputProfile, InstructionMap,
    MAX_CODE_IMAGE_BYTES, NativeWordInput, Relocation, RelocationKind, TranslationError,
    TranslationValidator, validate_translation,
};
pub use translation_validate::validate_translation_bytes;
