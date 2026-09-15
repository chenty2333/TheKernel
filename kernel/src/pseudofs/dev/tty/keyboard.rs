//! Bounded US console keymap for Linux evdev key codes. No input-device or VT
//! locks are taken here; callers apply the returned action after decoding.
use alloc::collections::VecDeque;

use axerrno::{AxError, AxResult};

#[derive(Clone, Copy)]
pub(crate) struct KeyboardTarget {
    pub(super) stamp: super::vt::ConsoleInputStamp,
    pub(super) mode: i32,
}

/// A decoded key is retained with its original recipient; retry never runs
/// the modifier/caps/hotkey state machine a second time.
#[derive(Clone, Copy)]
pub(crate) struct KeyboardInput {
    pub(super) stamp: super::vt::ConsoleInputStamp,
    pub(super) bytes: [u8; 8],
    pub(super) len: usize,
}

#[derive(Default)]
pub(crate) struct KeyboardState {
    shift: [bool; 2],
    ctrl: [bool; 2],
    alt: [bool; 2],
    caps: bool,
    num: bool,
    pending: VecDeque<KeyboardInput>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyAction {
    None,
    Bytes([u8; 8], usize),
    Switch(u16),
    Reboot,
}

impl KeyboardState {
    /// Admission is before hardware drain. No allocation is permitted once
    /// a batch has been consumed from the driver.
    pub(crate) fn prepare_batch(&mut self, limit: usize) -> AxResult<()> {
        if !self.pending.is_empty() {
            return Err(AxError::WouldBlock);
        }
        self.pending
            .try_reserve_exact(limit)
            .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn queue_input(&mut self, input: KeyboardInput) {
        // At most one packet per event in the admitted hardware batch.
        assert!(self.pending.len() < self.pending.capacity());
        self.pending.push_back(input);
    }

    /// Returns false on backpressure, leaving this and every later packet
    /// queued. A revoked recipient is discarded rather than retargeted.
    pub(crate) fn retry_pending(
        &mut self,
        mut deliver: impl FnMut(&KeyboardInput) -> AxResult<()>,
    ) -> AxResult<bool> {
        while let Some(input) = self.pending.front() {
            match deliver(input) {
                Ok(()) | Err(AxError::Interrupted) => {
                    self.pending.pop_front();
                }
                Err(AxError::WouldBlock) => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }

    pub(crate) fn key(&mut self, code: u16, value: i32, mode: i32) -> KeyAction {
        if !(0..=2).contains(&value) {
            return KeyAction::None;
        }
        let pressed = value != 0;
        match code {
            42 => self.shift[0] = pressed,
            54 => self.shift[1] = pressed,
            29 => self.ctrl[0] = pressed,
            97 => self.ctrl[1] = pressed,
            56 => self.alt[0] = pressed,
            100 => self.alt[1] = pressed,
            58 if value == 1 && matches!(mode, 1 | 3) => self.caps = !self.caps,
            69 if value == 1 && matches!(mode, 1 | 3) => self.num = !self.num,
            _ => {}
        }
        if mode == 4 {
            return KeyAction::None;
        } // K_OFF
        let ctrl = self.ctrl.contains(&true);
        let alt = self.alt.contains(&true);
        if mode == 0 {
            // K_RAW: PC set-1 make/break bytes, not cooked text.
            let scan = match code {
                1..=83 | 87..=88 => Some((false, code as u8)),
                96 => Some((true, 0x1c)),
                97 => Some((true, 0x1d)),
                98 => Some((true, 0x35)),
                100 => Some((true, 0x38)),
                102 => Some((true, 0x47)),
                103 => Some((true, 0x48)),
                104 => Some((true, 0x49)),
                105 => Some((true, 0x4b)),
                106 => Some((true, 0x4d)),
                107 => Some((true, 0x4f)),
                108 => Some((true, 0x50)),
                109 => Some((true, 0x51)),
                110 => Some((true, 0x52)),
                111 => Some((true, 0x53)),
                _ => None,
            };
            return scan.map_or(KeyAction::None, |(extended, scan)| {
                let mut bytes = [0; 8];
                bytes[0] = if extended {
                    0xe0
                } else {
                    scan | if pressed { 0 } else { 0x80 }
                };
                if extended {
                    bytes[1] = scan | if pressed { 0 } else { 0x80 };
                }
                KeyAction::Bytes(bytes, if extended { 2 } else { 1 })
            });
        }
        if mode == 2 {
            // K_MEDIUMRAW
            let mut bytes = [0; 8];
            bytes[0] = if pressed { 0 } else { 0x80 };
            if code < 128 {
                bytes[0] |= code as u8;
                return KeyAction::Bytes(bytes, 1);
            }
            bytes[1] = ((code >> 7) as u8) | 0x80;
            bytes[2] = (code as u8) | 0x80;
            return KeyAction::Bytes(bytes, 3);
        }
        if pressed && alt {
            let vt = match code {
                59..=68 => Some(code - 58),
                87..=88 => Some(code - 76),
                _ => None,
            };
            if let Some(vt) = vt {
                return KeyAction::Switch(vt);
            }
            if ctrl && code == 111 && value == 1 {
                return KeyAction::Reboot;
            }
        }
        if !pressed {
            return KeyAction::None;
        }
        let keypad = match code {
            71..=73 => Some(b'7' + (code - 71) as u8),
            75..=77 => Some(b'4' + (code - 75) as u8),
            79..=81 => Some(b'1' + (code - 79) as u8),
            82 => Some(b'0'),
            83 => Some(b'.'),
            _ => None,
        };
        if self.num ^ self.shift.contains(&true) {
            if let Some(byte) = keypad {
                let mut bytes = [0; 8];
                bytes[0] = byte;
                return KeyAction::Bytes(bytes, 1);
            }
        }
        let code = match code {
            71 => 102,
            72 => 103,
            73 => 104,
            75 => 105,
            77 => 106,
            79 => 107,
            80 => 108,
            81 => 109,
            82 => 110,
            83 => 111,
            other => other,
        };
        let sequence: &[u8] = match code {
            103 => b"\x1b[A",
            108 => b"\x1b[B",
            106 => b"\x1b[C",
            105 => b"\x1b[D",
            102 => b"\x1b[H",
            107 => b"\x1b[F",
            110 => b"\x1b[2~",
            111 => b"\x1b[3~",
            104 => b"\x1b[5~",
            109 => b"\x1b[6~",
            59 => b"\x1bOP",
            60 => b"\x1bOQ",
            61 => b"\x1bOR",
            62 => b"\x1bOS",
            63 => b"\x1b[15~",
            64 => b"\x1b[17~",
            65 => b"\x1b[18~",
            66 => b"\x1b[19~",
            67 => b"\x1b[20~",
            68 => b"\x1b[21~",
            87 => b"\x1b[23~",
            88 => b"\x1b[24~",
            96 => b"\r",
            98 => b"/",
            74 => b"-",
            78 => b"+",
            _ => b"",
        };
        let mut bytes = [0; 8];
        if !sequence.is_empty() {
            bytes[..sequence.len()].copy_from_slice(sequence);
            return KeyAction::Bytes(bytes, sequence.len());
        }
        const NORMAL: &[u8] =
            b"\0\x1b1234567890-=\x7f\tqwertyuiop[]\r\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
        const SHIFT: &[u8] =
            b"\0\x1b!@#$%^&*()_+\x7f\tQWERTYUIOP{}\r\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";
        let shift = self.shift.contains(&true);
        let Some(&plain) = NORMAL.get(code as usize) else {
            return KeyAction::None;
        };
        if plain == 0 {
            return KeyAction::None;
        }
        let shifted = if plain.is_ascii_alphabetic() {
            shift ^ self.caps
        } else {
            shift
        };
        let mut byte = if shifted { SHIFT[code as usize] } else { plain };
        if ctrl {
            byte = match byte {
                b'a'..=b'z' => byte - b'a' + 1,
                b'@'..=b'_' => byte & 0x1f,
                b' ' => 0,
                b'?' => 0x7f,
                _ => byte,
            };
        }
        let offset = usize::from(alt);
        if alt {
            bytes[0] = 0x1b;
        }
        bytes[offset] = byte;
        KeyAction::Bytes(bytes, offset + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn function_keys_and_numeric_keypad_are_not_silently_dropped() {
        let mut k = KeyboardState::default();
        assert_eq!(k.key(88, 1, 1), KeyAction::Bytes(*b"\x1b[24~\0\0\0", 5));
        assert_eq!(k.key(96, 1, 1), KeyAction::Bytes(*b"\r\0\0\0\0\0\0\0", 1));
        assert_eq!(k.key(72, 1, 1), KeyAction::Bytes(*b"\x1b[A\0\0\0\0\0", 3));
        k.key(69, 1, 1);
        assert_eq!(k.key(72, 1, 1), KeyAction::Bytes(*b"8\0\0\0\0\0\0\0", 1));
        k.key(42, 1, 1);
        assert_eq!(k.key(72, 1, 1), KeyAction::Bytes(*b"\x1b[A\0\0\0\0\0", 3));
    }

    #[test]
    fn raw_modes_deliver_hotkeys_and_do_not_toggle_caps() {
        for mode in [0, 2] {
            let mut k = KeyboardState::default();
            k.key(56, 1, mode);
            k.key(29, 1, mode);
            assert!(matches!(k.key(60, 1, mode), KeyAction::Bytes(_, _)));
            assert!(matches!(k.key(111, 1, mode), KeyAction::Bytes(_, _)));
            k.key(56, 0, mode);
            k.key(29, 0, mode);
            k.key(58, 1, mode);
            assert_eq!(
                k.key(30, 1, 1),
                KeyAction::Bytes([b'a', 0, 0, 0, 0, 0, 0, 0], 1)
            );
        }
    }

    #[test]
    fn pending_batch_retries_without_losing_suffix_or_redecoding_modifiers() {
        let mut k = KeyboardState::default();
        k.prepare_batch(3).unwrap();
        k.key(58, 1, 1); // Caps is decoded once, not once per retry.
        for code in [30, 48, 46] {
            let KeyAction::Bytes(bytes, len) = k.key(code, 1, 1) else {
                panic!("text expected");
            };
            k.queue_input(KeyboardInput {
                stamp: Default::default(),
                bytes,
                len,
            });
        }
        let mut output = alloc::vec::Vec::new();
        assert!(
            !k.retry_pending(|input| {
                if output.len() == 1 {
                    return Err(AxError::WouldBlock);
                }
                output.push(input.bytes[0]);
                Ok(())
            })
            .unwrap()
        );
        assert_eq!(k.prepare_batch(3), Err(AxError::WouldBlock));
        assert!(!k.retry_pending(|_| Err(AxError::WouldBlock)).unwrap());
        assert!(
            k.retry_pending(|input| {
                output.push(input.bytes[0]);
                Ok(())
            })
            .unwrap()
        );
        assert_eq!(output, b"ABC");
        assert_eq!(
            k.key(30, 1, 1),
            KeyAction::Bytes([b'A', 0, 0, 0, 0, 0, 0, 0], 1)
        );
        k.prepare_batch(3).unwrap();
    }

    #[test]
    fn revoked_pending_batch_is_dropped_not_retargeted() {
        let mut k = KeyboardState::default();
        k.prepare_batch(2).unwrap();
        for byte in [b'a', b'b'] {
            k.queue_input(KeyboardInput {
                stamp: Default::default(),
                bytes: [byte; 8],
                len: 1,
            });
        }
        let mut revoked = 0;
        assert!(
            k.retry_pending(|_| {
                revoked += 1;
                Err(AxError::Interrupted)
            })
            .unwrap()
        );
        assert_eq!(revoked, 2);
        assert!(k.pending.is_empty());
    }

    #[test]
    fn text_control_raw_and_off_modes_are_distinct() {
        let mut k = KeyboardState::default();
        assert_eq!(
            k.key(30, 1, 1),
            KeyAction::Bytes([b'a', 0, 0, 0, 0, 0, 0, 0], 1)
        );
        k.key(29, 1, 1);
        assert_eq!(
            k.key(46, 1, 1),
            KeyAction::Bytes([3, 0, 0, 0, 0, 0, 0, 0], 1)
        );
        assert_eq!(
            k.key(30, 1, 0),
            KeyAction::Bytes([0x1e, 0, 0, 0, 0, 0, 0, 0], 1)
        );
        assert_eq!(
            k.key(30, 0, 0),
            KeyAction::Bytes([0x9e, 0, 0, 0, 0, 0, 0, 0], 1)
        );
        assert_eq!(k.key(30, 1, 4), KeyAction::None);
    }
    #[test]
    fn hotkeys_and_independent_modifier_keys() {
        let mut k = KeyboardState::default();
        k.key(56, 1, 1);
        assert_eq!(k.key(60, 1, 1), KeyAction::Switch(2));
        k.key(29, 1, 1);
        assert_eq!(k.key(111, 1, 1), KeyAction::Reboot);
        assert_eq!(k.key(111, 2, 1), KeyAction::Bytes(*b"\x1b[3~\0\0\0\0", 4));
        k.key(56, 0, 1);
        k.key(29, 0, 1);
        k.key(42, 1, 1);
        k.key(54, 1, 1);
        k.key(42, 0, 1);
        assert_eq!(
            k.key(30, 1, 1),
            KeyAction::Bytes([b'A', 0, 0, 0, 0, 0, 0, 0], 1)
        );
    }
}
