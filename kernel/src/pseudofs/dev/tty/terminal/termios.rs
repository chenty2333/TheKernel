#![allow(dead_code)]

use core::ops::{Deref, DerefMut};

use axerrno::{AxError, AxResult};
use bytemuck::AnyBitPattern;
use linux_raw_sys::general::{
    B38400, CREAD, CS8, ECHO, ECHOCTL, ECHOE, ECHOK, ICANON, ICRNL, IGNCR, ISIG, ONLCR, OPOST,
    VEOF, VEOL, VERASE, VINTR, VKILL, VMIN, VQUIT, speed_t, tcflag_t,
};
use starry_signal::Signo;

const SUPPORTED_IFLAG_CHANGES: tcflag_t = ICRNL | IGNCR;
const SUPPORTED_OFLAG_CHANGES: tcflag_t = OPOST | ONLCR;
const SUPPORTED_LFLAG_CHANGES: tcflag_t = ICANON | ECHO | ISIG | ECHOE | ECHOK | ECHOCTL;

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

impl Default for Termios {
    fn default() -> Self {
        let mut result = Self {
            // Only advertise defaults which the PTY line discipline actually
            // enforces. Flow control and extended editing remain disabled
            // until their state machines exist.
            c_iflag: ICRNL,
            c_oflag: OPOST | ONLCR,
            c_cflag: B38400 | CS8 | CREAD,
            c_lflag: ICANON | ECHO | ISIG | ECHOE | ECHOK | ECHOCTL,
            c_line: 0,
            c_cc: [0; 19],
        };

        fn ctl(ch: u8) -> u8 {
            ch - 0x40
        }
        for (i, ch) in [
            (VINTR, ctl(b'C')),
            (VQUIT, ctl(b'\\')),
            (VERASE, b'\x7f'),
            (VKILL, ctl(b'U')),
            (VEOF, ctl(b'D')),
            (VEOL, b'\0'),
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
        self.c_iflag = termio.c_iflag as tcflag_t;
        self.c_oflag = termio.c_oflag as tcflag_t;
        self.c_cflag = termio.c_cflag as tcflag_t;
        self.c_lflag = termio.c_lflag as tcflag_t;
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
        ch == b'\n' || self.matches_special_char(VEOL, ch)
    }

    pub fn signo_for(&self, ch: u8) -> Option<Signo> {
        if self.matches_special_char(VINTR, ch) {
            Some(Signo::SIGINT)
        } else if self.matches_special_char(VQUIT, ch) {
            Some(Signo::SIGQUIT)
        } else {
            None
        }
    }

    fn validate_update(&self, current: &Self) -> AxResult<()> {
        if self.c_line != current.c_line {
            return Err(AxError::OperationNotSupported);
        }
        if (self.c_iflag ^ current.c_iflag) & !SUPPORTED_IFLAG_CHANGES != 0
            || (self.c_oflag ^ current.c_oflag) & !SUPPORTED_OFLAG_CHANGES != 0
            || self.c_cflag != current.c_cflag
            || (self.c_lflag ^ current.c_lflag) & !SUPPORTED_LFLAG_CHANGES != 0
        {
            return Err(AxError::OperationNotSupported);
        }
        // Canonical signal delivery is implemented. Noncanonical ISIG also
        // requires Linux's input/output flush and signal-byte consumption
        // rules, so reject that state instead of publishing a partial mode.
        if !self.has_lflag(ICANON) && self.has_lflag(ISIG) {
            return Err(AxError::OperationNotSupported);
        }

        for index in 0..self.c_cc.len() {
            let supported = matches!(
                index as u32,
                VINTR | VQUIT | VERASE | VKILL | VEOF | VEOL | VMIN
            );
            if !supported && self.c_cc[index] != current.c_cc[index] {
                return Err(AxError::OperationNotSupported);
            }
        }
        // VTIME is not implemented by the reader. It is deliberately outside
        // the supported index set above, so a nonzero request is rejected.
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

impl Default for Termios2 {
    fn default() -> Self {
        Self::new(Termios::default())
    }
}
impl Termios2 {
    pub fn new(termios: Termios) -> Self {
        Self {
            termios,
            c_ispeed: B38400,
            c_ospeed: B38400,
        }
    }

    pub fn from_termio(termio: Termio, current: &Self) -> Self {
        let mut result = *current;
        result.termios.apply_termio(termio);
        result
    }

    pub fn from_termios(termios: Termios, current: &Self) -> Self {
        let mut result = *current;
        result.termios = termios;
        result
    }

    pub fn as_termio(&self) -> Termio {
        self.termios.as_termio()
    }

    pub fn validate_update(&self, current: &Self) -> AxResult<()> {
        self.termios.validate_update(&current.termios)?;
        if self.c_ispeed != current.c_ispeed || self.c_ospeed != current.c_ospeed {
            return Err(AxError::OperationNotSupported);
        }
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
    fn unsupported_termios_changes_are_rejected_without_mutating_current() {
        let current = Termios2::default();

        let mut flow_control = current;
        flow_control.termios.c_iflag |= IXON;
        assert_eq!(
            flow_control.validate_update(&current),
            Err(AxError::OperationNotSupported)
        );

        let mut unsupported_cc = current;
        unsupported_cc.termios.c_cc[VWERASE as usize] = 0x17;
        assert_eq!(
            unsupported_cc.validate_update(&current),
            Err(AxError::OperationNotSupported)
        );

        let mut timed_read = current;
        timed_read.termios.c_cc[VTIME as usize] = 1;
        assert_eq!(
            timed_read.validate_update(&current),
            Err(AxError::OperationNotSupported)
        );
        assert_eq!(current.termios.c_iflag, ICRNL);
        assert_eq!(current.termios.c_cc[VTIME as usize], 0);

        let mut partial_noncanonical_signals = current;
        partial_noncanonical_signals.termios.c_lflag &= !ICANON;
        assert_eq!(
            partial_noncanonical_signals.validate_update(&current),
            Err(AxError::OperationNotSupported)
        );
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
