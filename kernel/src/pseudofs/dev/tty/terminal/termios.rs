#![allow(dead_code)]

use core::{
    mem::{align_of, offset_of, size_of},
    ops::{Deref, DerefMut},
};

use axerrno::{AxError, AxResult};
use bytemuck::AnyBitPattern;
use linux_raw_sys::general::{
    B38400, BOTHER, CBAUD, CIBAUD, CREAD, CS8, ECHO, ECHOCTL, ECHOE, ECHOK, ECHOKE, HUPCL, ICANON,
    ICRNL, IEXTEN, ISIG, IXON, ONLCR, OPOST, VDISCARD, VEOF, VEOL, VEOL2, VERASE, VINTR, VKILL,
    VLNEXT, VMIN, VQUIT, VREPRINT, VSTART, VSTOP, VSUSP, VWERASE, speed_t, tcflag_t,
};
#[cfg(test)]
use linux_raw_sys::general::{
    BRKINT, CSIZE, ECHONL, IGNCR, IMAXBEL, INPCK, ISTRIP, IUTF8, PARENB, TABDLY, TOSTOP,
};
use tk_linux_signal::Signo;

// Byte-stream TTYs retain serial framing and baud settings for ioctl round
// trips. No hardware parity/BREAK events are fabricated from ordinary bytes.
fn baud_rate(selector: u32, other: speed_t) -> speed_t {
    const BASE: [u32; 16] = [
        0, 50, 75, 110, 134, 150, 200, 300, 600, 1200, 1800, 2400, 4800, 9600, 19200, 38400,
    ];
    const EXTENDED: [u32; 15] = [
        57600, 115200, 230400, 460800, 500000, 576000, 921600, 1000000, 1152000, 1500000, 2000000,
        2500000, 3000000, 3500000, 4000000,
    ];
    match selector {
        0..=15 => BASE[selector as usize],
        BOTHER => other,
        4097..=4111 => EXTENDED[(selector - 4097) as usize],
        _ => other,
    }
}

const _: () = {
    assert!(size_of::<Termio>() == 18);
    assert!(align_of::<Termio>() == 2);
    assert!(offset_of!(Termio, c_cc) == 9);
    assert!(size_of::<Termios>() == 36);
    assert!(align_of::<Termios>() == 4);
    assert!(offset_of!(Termios, c_cc) == 17);
    assert!(size_of::<Termios2>() == 44);
    assert!(align_of::<Termios2>() == 4);
    assert!(offset_of!(Termios2, c_ispeed) == 36);
};

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct Termio {
    c_iflag: u16,
    c_oflag: u16,
    c_cflag: u16,
    c_lflag: u16,
    c_line: u8,
    c_cc: [u8; 8usize],
}

impl Termio {
    /// Decodes a complete Linux `termio` image from its wire representation.
    ///
    /// The syscall layer deliberately reads bytes instead of asking the
    /// usercopy helper to reinterpret a Rust struct.  This keeps the ABI
    /// padding byte out of the kernel value and makes the accepted layout
    /// explicit at the boundary.
    pub(crate) fn from_user_bytes(bytes: [u8; size_of::<Self>()]) -> Self {
        let read_u16 = |offset| u16::from_ne_bytes(bytes[offset..][..2].try_into().unwrap());
        let mut c_cc = [0; 8];
        let c_cc_len = c_cc.len();
        c_cc.copy_from_slice(&bytes[offset_of!(Self, c_cc)..][..c_cc_len]);
        Self {
            c_iflag: read_u16(offset_of!(Self, c_iflag)),
            c_oflag: read_u16(offset_of!(Self, c_oflag)),
            c_cflag: read_u16(offset_of!(Self, c_cflag)),
            c_lflag: read_u16(offset_of!(Self, c_lflag)),
            c_line: bytes[offset_of!(Self, c_line)],
            c_cc,
        }
    }

    /// Encodes the complete Linux `termio` image with its trailing padding
    /// byte explicitly zeroed before copyout.
    pub(crate) fn to_user_bytes(self) -> [u8; size_of::<Self>()] {
        let mut bytes = [0u8; size_of::<Self>()];
        bytes[offset_of!(Self, c_iflag)..][..2].copy_from_slice(&self.c_iflag.to_ne_bytes());
        bytes[offset_of!(Self, c_oflag)..][..2].copy_from_slice(&self.c_oflag.to_ne_bytes());
        bytes[offset_of!(Self, c_cflag)..][..2].copy_from_slice(&self.c_cflag.to_ne_bytes());
        bytes[offset_of!(Self, c_lflag)..][..2].copy_from_slice(&self.c_lflag.to_ne_bytes());
        bytes[offset_of!(Self, c_line)] = self.c_line;
        bytes[offset_of!(Self, c_cc)..][..self.c_cc.len()].copy_from_slice(&self.c_cc);
        bytes
    }
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct Termios {
    c_iflag: tcflag_t,
    c_oflag: tcflag_t,
    c_cflag: tcflag_t,
    c_lflag: tcflag_t,
    c_line: u8,
    c_cc: [u8; 19usize],
}

impl Termios {
    /// Decodes a complete Linux `termios` image from bytes at the UAPI
    /// boundary.  Any representation padding is intentionally ignored.
    pub(crate) fn from_user_bytes(bytes: [u8; size_of::<Self>()]) -> Self {
        let read_u32 = |offset| u32::from_ne_bytes(bytes[offset..][..4].try_into().unwrap());
        let mut c_cc = [0; 19];
        let c_cc_len = c_cc.len();
        c_cc.copy_from_slice(&bytes[offset_of!(Self, c_cc)..][..c_cc_len]);
        Self {
            c_iflag: read_u32(offset_of!(Self, c_iflag)),
            c_oflag: read_u32(offset_of!(Self, c_oflag)),
            c_cflag: read_u32(offset_of!(Self, c_cflag)),
            c_lflag: read_u32(offset_of!(Self, c_lflag)),
            c_line: bytes[offset_of!(Self, c_line)],
            c_cc,
        }
    }

    /// Encodes the complete Linux `termios` image. Any ABI tail padding is
    /// zeroed rather than copied from a Rust struct representation.
    pub(crate) fn to_user_bytes(self) -> [u8; size_of::<Self>()] {
        let mut bytes = [0u8; size_of::<Self>()];
        bytes[offset_of!(Self, c_iflag)..][..size_of::<tcflag_t>()]
            .copy_from_slice(&self.c_iflag.to_ne_bytes());
        bytes[offset_of!(Self, c_oflag)..][..size_of::<tcflag_t>()]
            .copy_from_slice(&self.c_oflag.to_ne_bytes());
        bytes[offset_of!(Self, c_cflag)..][..size_of::<tcflag_t>()]
            .copy_from_slice(&self.c_cflag.to_ne_bytes());
        bytes[offset_of!(Self, c_lflag)..][..size_of::<tcflag_t>()]
            .copy_from_slice(&self.c_lflag.to_ne_bytes());
        bytes[offset_of!(Self, c_line)] = self.c_line;
        bytes[offset_of!(Self, c_cc)..][..self.c_cc.len()].copy_from_slice(&self.c_cc);
        bytes
    }
}

impl Default for Termios {
    fn default() -> Self {
        let mut result = Self {
            c_iflag: ICRNL | IXON,
            c_oflag: OPOST | ONLCR,
            c_cflag: B38400 | CS8 | CREAD | HUPCL,
            c_lflag: ICANON | ECHO | ISIG | ECHOE | ECHOK | ECHOCTL | IEXTEN | ECHOKE,
            c_line: 0,
            c_cc: [0; 19],
        };

        fn ctl(ch: u8) -> u8 {
            ch - 0x40
        }
        for (i, ch) in [
            (VINTR, ctl(b'C')),
            (VQUIT, ctl(b'\\')),
            (VSUSP, ctl(b'Z')),
            (VERASE, b'\x7f'),
            (VKILL, ctl(b'U')),
            (VEOF, ctl(b'D')),
            (VEOL, b'\0'),
            (VSTART, ctl(b'Q')),
            (VSTOP, ctl(b'S')),
            (VWERASE, ctl(b'W')),
            (VREPRINT, ctl(b'R')),
            (VLNEXT, ctl(b'V')),
            (VDISCARD, ctl(b'O')),
            (VMIN, 1),
        ] {
            result.c_cc[i as usize] = ch;
        }

        result
    }
}

impl Termios {
    pub fn as_termio(&self) -> Termio {
        let mut c_cc = [0; 8];
        c_cc.copy_from_slice(&self.c_cc[..8]);
        Termio {
            c_iflag: self.c_iflag as u16,
            c_oflag: self.c_oflag as u16,
            c_cflag: self.c_cflag as u16,
            c_lflag: self.c_lflag as u16,
            c_line: self.c_line,
            c_cc,
        }
    }

    pub fn apply_termio(&mut self, termio: Termio) {
        self.c_iflag = (self.c_iflag & !0xffff) | termio.c_iflag as tcflag_t;
        self.c_oflag = (self.c_oflag & !0xffff) | termio.c_oflag as tcflag_t;
        self.c_cflag = (self.c_cflag & !0xffff) | termio.c_cflag as tcflag_t;
        self.c_lflag = (self.c_lflag & !0xffff) | termio.c_lflag as tcflag_t;
        self.c_line = termio.c_line;
        self.c_cc[..8].copy_from_slice(&termio.c_cc);
    }

    pub fn special_char(&self, index: u32) -> u8 {
        self.c_cc[index as usize]
    }

    pub fn matches_special_char(&self, index: u32, ch: u8) -> bool {
        let configured = self.special_char(index);
        configured != 0 && configured == ch
    }

    pub fn has_iflag(&self, flag: u32) -> bool {
        self.c_iflag & flag != 0
    }

    pub fn has_oflag(&self, flag: u32) -> bool {
        self.c_oflag & flag != 0
    }

    pub fn output_tab_expansion(&self) -> bool {
        self.c_oflag & linux_raw_sys::general::TABDLY == linux_raw_sys::general::TAB3
    }

    pub fn has_cflag(&self, flag: u32) -> bool {
        self.c_cflag & flag != 0
    }

    pub fn has_lflag(&self, flag: u32) -> bool {
        self.c_lflag & flag != 0
    }

    pub fn echo(&self) -> bool {
        self.has_lflag(ECHO)
    }

    pub fn canonical(&self) -> bool {
        self.has_lflag(ICANON)
    }

    pub fn is_eol(&self, ch: u8) -> bool {
        ch == b'\n'
            || self.matches_special_char(VEOL, ch)
            || (self.has_lflag(IEXTEN) && self.matches_special_char(VEOL2, ch))
    }

    pub fn signo_for(&self, ch: u8) -> Option<Signo> {
        if self.matches_special_char(VINTR, ch) {
            Some(Signo::SIGINT)
        } else if self.matches_special_char(VQUIT, ch) {
            Some(Signo::SIGQUIT)
        } else if self.matches_special_char(VSUSP, ch) {
            Some(Signo::SIGTSTP)
        } else {
            None
        }
    }

    fn validate_update(&self, current: &Self) -> AxResult<()> {
        if self.c_line != current.c_line {
            return Err(AxError::OperationNotSupported);
        }
        // Linux generic termios stores reserved bits. Preserve those for ioctl
        // round trips, without claiming that they enable any behavior. Known
        // modes whose semantics are not implemented still fail explicitly.
        use linux_raw_sys::general::{ECHOPRT, EXTPROC, FLUSHO, IXOFF, PARMRK, PENDIN};
        if (self.c_iflag ^ current.c_iflag) & (PARMRK | IXOFF) != 0
            || (self.c_lflag ^ current.c_lflag) & (ECHOPRT | FLUSHO | PENDIN | EXTPROC) != 0
        {
            return Err(AxError::OperationNotSupported);
        }
        // N_TTY ignores unimplemented/reserved c_cc slots, but the ABI still
        // stores them (including legacy VSWTC). They are not mode flags.
        Ok(())
    }
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct Termios2 {
    termios: Termios,
    c_ispeed: speed_t,
    c_ospeed: speed_t,
}

impl Termios2 {
    /// Decodes a complete Linux `termios2` image from its fixed wire layout.
    /// The reserved representation bytes in the embedded `termios` image are
    /// not interpreted as Rust state.
    pub(crate) fn from_user_bytes(bytes: [u8; size_of::<Self>()]) -> Self {
        let termios = Termios::from_user_bytes(
            bytes[..size_of::<Termios>()]
                .try_into()
                .expect("termios2 wire prefix has the Linux termios size"),
        );
        let read_u32 = |offset| u32::from_ne_bytes(bytes[offset..][..4].try_into().unwrap());
        let mut result = Self {
            termios,
            c_ispeed: read_u32(offset_of!(Self, c_ispeed)) as speed_t,
            c_ospeed: read_u32(offset_of!(Self, c_ospeed)) as speed_t,
        };
        result.refresh_baud_rates();
        result
    }

    /// Encodes `termios2` from field bytes and keeps every ABI byte
    /// deterministic, including any representation padding.
    pub(crate) fn to_user_bytes(self) -> [u8; size_of::<Self>()] {
        let mut bytes = [0u8; size_of::<Self>()];
        let termios = self.termios.to_user_bytes();
        let termios_offset = offset_of!(Self, termios);
        bytes[termios_offset..][..termios.len()].copy_from_slice(&termios);
        bytes[offset_of!(Self, c_ispeed)..][..size_of::<speed_t>()]
            .copy_from_slice(&self.c_ispeed.to_ne_bytes());
        bytes[offset_of!(Self, c_ospeed)..][..size_of::<speed_t>()]
            .copy_from_slice(&self.c_ospeed.to_ne_bytes());
        bytes
    }
}

impl Default for Termios2 {
    fn default() -> Self {
        Self::new(Termios::default())
    }
}
impl Termios2 {
    pub fn new(termios: Termios) -> Self {
        Self {
            termios,
            // Linux termios2 carries numeric baud rates in c_ispeed/c_ospeed
            // (userspace reads and writes 38400, not the B38400 selector kept
            // in c_cflag's CBAUD bits).  Anything else breaks the
            // TCGETS2/TCSETS2 round trip every libc performs.
            c_ispeed: 38400,
            c_ospeed: 38400,
        }
    }

    pub fn from_termio(termio: Termio, current: &Self) -> Self {
        let mut result = *current;
        result.termios.apply_termio(termio);
        result.refresh_baud_rates();
        result
    }

    pub fn from_termios(termios: Termios, current: &Self) -> Self {
        let mut result = *current;
        result.termios = termios;
        result.refresh_baud_rates();
        result
    }

    pub fn as_termio(&self) -> Termio {
        self.termios.as_termio()
    }

    fn refresh_baud_rates(&mut self) {
        self.c_ospeed = baud_rate(self.c_cflag & CBAUD, self.c_ospeed);
        let input = (self.c_cflag & CIBAUD) >> 16;
        self.c_ispeed = if input == 0 {
            self.c_ospeed
        } else {
            baud_rate(input, self.c_ispeed)
        };
    }

    pub fn validate_update(&self, current: &Self) -> AxResult<()> {
        self.termios.validate_update(&current.termios)?;
        Ok(())
    }
}

#[cfg(test)]
impl Termios2 {
    pub(super) fn set_canonical_for_test(&mut self, enabled: bool) {
        if enabled {
            self.termios.c_lflag |= ICANON;
        } else {
            self.termios.c_lflag &= !ICANON;
        }
    }

    pub(super) fn set_special_char_for_test(&mut self, index: u32, value: u8) {
        self.termios.c_cc[index as usize] = value;
    }

    pub(super) fn set_output_processing_for_test(&mut self, opost: bool, onlcr: bool) {
        self.termios.c_oflag &= !(OPOST | ONLCR);
        if opost {
            self.termios.c_oflag |= OPOST;
        }
        if onlcr {
            self.termios.c_oflag |= ONLCR;
        }
    }
}

impl Deref for Termios2 {
    type Target = Termios;

    fn deref(&self) -> &Self::Target {
        &self.termios
    }
}

impl DerefMut for Termios2 {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.termios
    }
}

#[cfg(test)]
mod tests {
    use linux_raw_sys::general::{IXON, VTIME, VWERASE};

    use super::*;

    #[test]
    fn defaults_and_common_stty_modes_are_supported() {
        let current = Termios2::default();
        assert_eq!(current.special_char(VMIN), 1);
        assert!(current.has_iflag(IXON));
        assert!(current.has_lflag(IEXTEN | ECHOKE));
        let mut next = current;
        next.termios.c_lflag |= TOSTOP | ECHONL;
        next.termios.c_iflag |= IMAXBEL | IUTF8;
        next.termios.c_cc[VWERASE as usize] = 0x17;
        next.termios.c_cflag = (next.termios.c_cflag & !(CBAUD | CSIZE))
            | linux_raw_sys::general::B115200
            | linux_raw_sys::general::CS7;
        next = Termios2::from_termios(next.termios, &current);
        assert_eq!(next.validate_update(&current), Ok(()));
        assert_eq!(next.c_ispeed, 115200);
        assert_eq!(next.c_ospeed, 115200);
        assert_eq!(next.c_cflag & CSIZE, linux_raw_sys::general::CS7);
        next.termios.c_cflag = (next.termios.c_cflag & !(CBAUD | CIBAUD)) | BOTHER | (BOTHER << 16);
        next.c_ispeed = 12345;
        next.c_ospeed = 56789;
        let restored = Termios2::from_user_bytes(next.to_user_bytes());
        assert_eq!(restored.validate_update(&current), Ok(()));
        assert_eq!(restored.c_ispeed, 12345);
        assert_eq!(restored.c_ospeed, 56789);
    }

    #[test]
    fn implemented_termios_changes_validate_atomically() {
        let current = Termios2::default();
        let mut next = current;
        next.termios.c_iflag ^= IGNCR;
        next.termios.c_oflag &= !(OPOST | ONLCR);
        next.termios.c_lflag &= !(ICANON | ECHO | ISIG);
        next.termios.c_cc[VMIN as usize] = 3;

        assert_eq!(next.validate_update(&current), Ok(()));
    }

    #[test]
    fn reserved_flags_round_trip_without_enabling_known_unsupported_modes() {
        let current = Termios2::default();
        let mut next = current;
        next.termios.c_iflag |= 1 << 31;
        next.termios.c_oflag |= 1 << 31;
        next.termios.c_lflag |= 1 << 31;
        next.termios.c_cflag |= 1 << 23;
        assert_eq!(next.validate_update(&current), Ok(()));
        let restored = Termios2::from_user_bytes(next.to_user_bytes());
        assert_eq!(restored.c_iflag, next.c_iflag);
        assert_eq!(restored.c_oflag, next.c_oflag);
        assert_eq!(restored.c_lflag, next.c_lflag);
        assert_eq!(restored.c_cflag, next.c_cflag);
    }

    #[test]
    fn unsupported_termios_changes_are_rejected_without_mutating_current() {
        let current = Termios2::default();

        let mut flow_control = current;
        flow_control.termios.c_iflag |= linux_raw_sys::general::IXOFF;
        assert_eq!(
            flow_control.validate_update(&current),
            Err(AxError::OperationNotSupported)
        );

        let mut unsupported_cc = current;
        unsupported_cc.termios.c_cc[VWERASE as usize] = 0x17;
        assert_eq!(unsupported_cc.validate_update(&current), Ok(()));

        let mut timed_read = current;
        timed_read.termios.c_cc[VTIME as usize] = 1;
        assert_eq!(timed_read.validate_update(&current), Ok(()));
        assert_eq!(current.termios.c_iflag, ICRNL | IXON);
        assert_eq!(current.termios.c_cc[VTIME as usize], 0);

        let mut cbreak = current;
        cbreak.termios.c_lflag &= !ICANON;
        assert_eq!(cbreak.validate_update(&current), Ok(()));
    }

    #[test]
    fn cpython_pyrepl_prepare_and_restore_termios_are_supported() {
        use linux_raw_sys::general::{CSIZE, IEXTEN, INPCK, ISTRIP, PARENB};
        let current = Termios2::default();
        let mut raw = current;
        raw.termios.c_iflag &= !(INPCK | ISTRIP | IXON);
        raw.termios.c_iflag |= BRKINT;
        raw.termios.c_oflag &= !OPOST;
        raw.termios.c_cflag &= !(CSIZE | PARENB);
        raw.termios.c_cflag |= CS8;
        raw.termios.c_lflag &= !(ICANON | ECHO | IEXTEN);
        raw.termios.c_lflag |= ISIG;
        raw.termios.c_cc[VMIN as usize] = 1;
        raw.termios.c_cc[VTIME as usize] = 0;
        assert_eq!(raw.validate_update(&current), Ok(()));
        assert_eq!(current.validate_update(&raw), Ok(()));
    }

    #[test]
    fn disabled_control_char_does_not_match_nul_input() {
        let termios = Termios2::default();
        assert!(!termios.is_eol(0));
        assert_eq!(termios.signo_for(0), None);
        assert!(!termios.matches_special_char(VEOF, 0));
        assert!(!termios.matches_special_char(VERASE, 0));
    }
}
