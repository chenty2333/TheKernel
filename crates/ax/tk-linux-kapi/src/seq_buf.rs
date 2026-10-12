// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Translation of Linux 7.2.3 `lib/seq_buf.c` and the inline helpers of
//! `include/linux/seq_buf.h` (GPL-2.0 upstream), the subset used by the DRM
//! and i915 state dumps: `seq_buf_printf`/`vprintf`, `puts`, `putc`,
//! `putmem`, `putmem_hex`, `clear`, `str`, `has_overflowed` and the length
//! helpers.
//!
//! `struct seq_buf` is represented by a borrowed byte slice (`size` is the
//! slice length) plus the Linux `len` counter. An overflowing write sets
//! `len` to `size + 1`, exactly as `seq_buf_set_overflow()` does, so later
//! writes keep failing until the buffer is cleared.

use alloc::{string::String, vec::Vec};

use crate::print::{CFormatArg, format_c_message};

/// `MAX_MEMHEX_BYTES` from lib/seq_buf.c.
pub const MAX_MEMHEX_BYTES: usize = 8;

/// Linux `struct seq_buf`: a bounded text buffer with an overflow flag.
pub struct SeqBuf<'a> {
    buffer: &'a mut [u8],
    len: usize,
}

impl<'a> SeqBuf<'a> {
    /// Linux `seq_buf_init()`.
    pub fn new(buffer: &'a mut [u8]) -> Self {
        let mut seq = Self { buffer, len: 0 };
        seq.clear();
        seq
    }

    /// Linux `seq_buf_clear()`.
    pub fn clear(&mut self) {
        self.len = 0;
        if let Some(first) = self.buffer.first_mut() {
            *first = 0;
        }
    }

    /// Linux `seq_buf_has_overflowed()`.
    pub fn has_overflowed(&self) -> bool {
        self.len > self.buffer.len()
    }

    /// Linux `seq_buf_buffer_left()`.
    pub fn buffer_left(&self) -> usize {
        if self.has_overflowed() {
            0
        } else {
            self.buffer.len() - self.len
        }
    }

    /// Linux `seq_buf_used()`: bytes actually stored.
    pub fn used(&self) -> usize {
        self.len.min(self.buffer.len())
    }

    /// Linux `seq_buf_str()`: NUL-terminate the contents in place and return
    /// them without the terminator. Returns an empty slice for a zero-sized
    /// buffer, as the WARN_ON() path does.
    pub fn str(&mut self) -> &[u8] {
        if self.buffer.is_empty() {
            return &[];
        }
        let end = if self.buffer_left() > 0 {
            self.len
        } else {
            self.buffer.len() - 1
        };
        self.buffer[end] = 0;
        &self.buffer[..end]
    }

    /// Linux `seq_buf_vprintf()`/`seq_buf_printf()` with the formatted text
    /// already rendered. Returns zero on success or -1 on overflow.
    pub fn vprintf(&mut self, text: &str) -> i32 {
        if self.len < self.buffer.len() {
            let n = text.len();
            if self.len + n < self.buffer.len() {
                self.buffer[self.len..self.len + n].copy_from_slice(text.as_bytes());
                self.len += n;
                return 0;
            }
        }
        self.set_overflow();
        -1
    }

    /// Linux `seq_buf_printf()` for the C format subset understood by the
    /// shared DRM printer.
    pub fn printf(&mut self, format: &str, args: &[&dyn CFormatArg]) -> i32 {
        let text: String = format_c_message(format, args);
        self.vprintf(&text)
    }

    /// Linux `seq_buf_puts()`: the C string ends at the first NUL, and the
    /// terminator must fit even though it is not counted.
    pub fn puts(&mut self, text: &str) -> i32 {
        let bytes = text.as_bytes();
        let n = bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len());
        self.put_bytes_with_nul(&bytes[..n])
    }

    /// Linux `seq_buf_putc()`.
    pub fn putc(&mut self, byte: u8) -> i32 {
        self.putmem(core::slice::from_ref(&byte))
    }

    /// Linux `seq_buf_putmem()`.
    pub fn putmem(&mut self, mem: &[u8]) -> i32 {
        if self.can_fit(mem.len()) {
            self.buffer[self.len..self.len + mem.len()].copy_from_slice(mem);
            self.len += mem.len();
            return 0;
        }
        self.set_overflow();
        -1
    }

    /// Linux `seq_buf_putmem_hex()`: each chunk of up to eight bytes is
    /// printed as hex digit pairs in reverse (little-endian) order, followed
    /// by one space.
    pub fn putmem_hex(&mut self, mem: &[u8]) -> i32 {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut rest = mem;
        while !rest.is_empty() {
            let chunk_len = rest.len().min(MAX_MEMHEX_BYTES);
            let chunk = &rest[..chunk_len];
            let mut out: Vec<u8> = Vec::with_capacity(chunk_len * 2 + 1);
            for &byte in chunk.iter().rev() {
                out.push(HEX[(byte >> 4) as usize]);
                out.push(HEX[(byte & 0x0f) as usize]);
            }
            out.push(b' ');
            self.putmem(&out);
            if self.has_overflowed() {
                return -1;
            }
            rest = &rest[chunk_len..];
        }
        0
    }

    fn can_fit(&self, len: usize) -> bool {
        self.len + len <= self.buffer.len()
    }

    fn put_bytes_with_nul(&mut self, bytes: &[u8]) -> i32 {
        // The trailing NUL is required to fit but is not counted.
        if self.can_fit(bytes.len() + 1) {
            self.buffer[self.len..self.len + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len();
            return 0;
        }
        self.set_overflow();
        -1
    }

    fn set_overflow(&mut self) {
        self.len = self.buffer.len() + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printf_appends_and_str_terminates_in_place() {
        let mut storage = [0xffu8; 32];
        let mut seq = SeqBuf::new(&mut storage);
        assert_eq!(seq.printf("x=%u", &[&42u32 as &dyn CFormatArg]), 0);
        assert_eq!(seq.puts(" ok"), 0);
        assert_eq!(seq.used(), 7);
        assert_eq!(seq.str(), b"x=42 ok");
        assert_eq!(storage[7], 0);
    }

    #[test]
    fn overflow_sets_flag_and_blocks_later_writes_until_clear() {
        let mut storage = [0u8; 8];
        let mut seq = SeqBuf::new(&mut storage);
        assert_eq!(seq.puts("1234567"), 0);
        assert_eq!(seq.buffer_left(), 1);
        assert_eq!(seq.puts("x"), -1);
        assert!(seq.has_overflowed());
        assert_eq!(seq.putc(b'y'), -1);
        assert_eq!(seq.used(), 8);
        seq.clear();
        assert!(!seq.has_overflowed());
        assert_eq!(seq.buffer_left(), 8);
        assert_eq!(seq.putc(b'z'), 0);
    }

    #[test]
    fn puts_reserves_one_byte_for_the_terminator() {
        let mut storage = [0u8; 4];
        let mut seq = SeqBuf::new(&mut storage);
        assert_eq!(seq.puts("abc"), 0);
        assert_eq!(seq.buffer_left(), 1);
        assert!(!seq.has_overflowed());
        assert_eq!(seq.str(), b"abc");
    }

    #[test]
    fn puts_stops_at_embedded_nul_like_strlen() {
        let mut storage = [0u8; 16];
        let mut seq = SeqBuf::new(&mut storage);
        assert_eq!(seq.puts("ab\0cd"), 0);
        assert_eq!(seq.str(), b"ab");
    }

    #[test]
    fn putmem_hex_prints_chunks_in_reverse_byte_order() {
        let mut storage = [0u8; 64];
        let mut seq = SeqBuf::new(&mut storage);
        assert_eq!(seq.putmem_hex(&[0x12, 0x34, 0xab]), 0);
        assert_eq!(seq.str(), b"ab3412 ");
        let mut seq = SeqBuf::new(&mut storage);
        let nine: [u8; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 9];
        assert_eq!(seq.putmem_hex(&nine), 0);
        assert_eq!(seq.str(), b"0807060504030201 09 ");
    }

    #[test]
    fn zero_sized_buffer_yields_empty_string() {
        let mut storage: [u8; 0] = [];
        let mut seq = SeqBuf::new(&mut storage);
        assert_eq!(seq.puts("a"), -1);
        assert_eq!(seq.str(), b"");
    }
}
