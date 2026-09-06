#![cfg(all(unix, target_arch = "x86_64"))]

use std::ffi::c_void;
use std::ptr;

use axcbpf::{
    InputProfile, Instruction, NativeWordInput, Program, TranslationValidator, opcode,
    validate_translation_bytes,
};

unsafe extern "C" {
    fn mmap(
        address: *mut c_void,
        length: usize,
        protection: i32,
        flags: i32,
        file: i32,
        offset: isize,
    ) -> *mut c_void;
    fn mprotect(address: *mut c_void, length: usize, protection: i32) -> i32;
    fn munmap(address: *mut c_void, length: usize) -> i32;
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const PROT_EXEC: i32 = 4;
const MAP_PRIVATE: i32 = 2;
const MAP_ANONYMOUS: i32 = 0x20;

fn statement(code: u16, k: u32) -> Instruction {
    Instruction::statement(code, k)
}

#[test]
fn emits_and_validates_big_endian_byte_input() {
    let program =
        Program::verify(&[statement(opcode::LD_W_ABS, 0), statement(opcode::RET_A, 0)]).unwrap();
    let image = program.translate().unwrap();

    assert_eq!(image.profile(), InputProfile::BigEndianBytes);
    TranslationValidator::validate(&image).unwrap();
    validate_translation_bytes(
        image.bytes(),
        program.instructions(),
        InputProfile::BigEndianBytes,
    )
    .unwrap();
    assert_eq!(run(image.bytes(), &[0x12, 0x34, 0x56, 0x78]), 0x1234_5678);
}

#[test]
fn native_aligned_word_profile_remains_available_to_adapters() {
    let program =
        Program::verify(&[statement(opcode::LD_W_ABS, 4), statement(opcode::RET_A, 0)]).unwrap();
    let image = program
        .translate_with_profile(InputProfile::NativeAlignedWords)
        .unwrap();
    let bytes = [0, 0, 0, 0, 0x78, 0x56, 0x34, 0x12];
    let input = NativeWordInput::new(&bytes);

    assert_eq!(image.evaluate(&input), program.evaluate(&input));
    assert_eq!(run(image.bytes(), &bytes), 0x1234_5678);
}

fn run(bytes: &[u8], input: &[u8]) -> u32 {
    let page_size = 4096;
    let allocation_size = bytes.len().div_ceil(page_size) * page_size;
    let memory = unsafe {
        mmap(
            ptr::null_mut(),
            allocation_size,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(memory, (-1_isize) as *mut c_void);
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), memory.cast::<u8>(), bytes.len());
    }
    assert_eq!(
        unsafe { mprotect(memory, allocation_size, PROT_READ | PROT_EXEC) },
        0
    );
    let function: extern "C" fn(*const u8, u32) -> u32 = unsafe { core::mem::transmute(memory) };
    let result = function(input.as_ptr(), input.len() as u32);
    assert_eq!(unsafe { munmap(memory, allocation_size) }, 0);
    result
}
