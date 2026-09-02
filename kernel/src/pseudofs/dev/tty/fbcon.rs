//! Small, bounded framebuffer console for Linux virtual terminals.
//!
//! The serial console remains the authoritative early/debug console.  This
//! module mirrors terminal output into per-VT character cells once fbdev has
//! supplied a scanout surface.  The VT arbiter serializes the decision to
//! render with KD_GRAPHICS transitions; fbcon then holds the cell lock across
//! the bounded draw so a concurrent writer cannot make the presented cells
//! inconsistent with that decision.

use alloc::{boxed::Box, vec::Vec};
use core::time::Duration;

use axsync::Mutex;

use crate::pseudofs::dev::fb;

const VT_COUNT: usize = 63;
const MAX_COLS: usize = 160;
const MAX_ROWS: usize = 64;
const CELL_WIDTH: usize = 8;
const CELL_HEIGHT: usize = 16;

struct Screen {
    cells: Box<[u8; MAX_COLS * MAX_ROWS]>,
    row: usize,
    col: usize,
    escape: u8,
}

impl Screen {
    fn try_new() -> Option<Self> {
        Some(Self {
            cells: Box::try_new([b' '; MAX_COLS * MAX_ROWS]).ok()?,
            row: 0,
            col: 0,
            escape: 0,
        })
    }

    fn clear(&mut self) {
        self.cells.fill(b' ');
        self.row = 0;
        self.col = 0;
        self.escape = 0;
    }

    fn put(&mut self, byte: u8, cols: usize, rows: usize) {
        match self.escape {
            1 => {
                self.escape = u8::from(byte == b'[') * 2;
                return;
            }
            2 => {
                if (0x40..=0x7e).contains(&byte) {
                    self.escape = 0;
                }
                return;
            }
            _ => {}
        }
        if byte == 0x1b {
            self.escape = 1;
            return;
        }
        match byte {
            b'\r' => self.col = 0,
            b'\n' => {
                self.col = 0;
                self.row += 1;
            }
            8 => self.col = self.col.saturating_sub(1),
            b'\t' => {
                self.col = (self.col + 8) & !7;
                if self.col >= cols {
                    self.col = 0;
                    self.row += 1;
                }
            }
            0x20..=0x7e => {
                self.cells[self.row * MAX_COLS + self.col] = byte;
                self.col += 1;
                if self.col == cols {
                    self.col = 0;
                    self.row += 1;
                }
            }
            _ => {}
        }
        if self.row >= rows {
            self.cells.copy_within(MAX_COLS..MAX_COLS * rows, 0);
            self.cells[MAX_COLS * (rows - 1)..MAX_COLS * rows].fill(b' ');
            self.row = rows - 1;
        }
    }
}

struct Console {
    screens: Vec<Screen>,
}

impl Console {
    fn try_new() -> Option<Self> {
        let mut screens = Vec::new();
        screens.try_reserve_exact(VT_COUNT).ok()?;
        for _ in 0..VT_COUNT {
            screens.push(Screen::try_new()?);
        }
        Some(Self { screens })
    }
}

// Full-frame drawing can touch slow MMIO.  Do not disable interrupts while
// holding the screen snapshot across that operation.
static FBCON: Mutex<Option<Console>> = Mutex::new(None);

/// A full-screen repaint costs tens of milliseconds, so the write path
/// coalesces repaints instead of paying one per write call.  The character
/// cells are the authoritative state: writes between repaints lose nothing,
/// the next repaint draws from the latest cells.
const WRITE_PRESENT_INTERVAL: Duration = Duration::from_millis(33);

struct PresentState {
    /// Monotonic time when the trailing task last started a repaint.
    last: Duration,
    /// Unpresented writes exist; this VT needs the next repaint.
    trailing_vt: Option<u16>,
    /// The trailing repaint task is alive.
    trailing_running: bool,
}

static PRESENT: Mutex<PresentState> = Mutex::new(PresentState {
    last: Duration::ZERO,
    trailing_vt: None,
    trailing_running: false,
});

/// Enables framebuffer rendering after fbdev owns the scanout memory.
/// Allocation is bounded and happens only at setup; an allocation failure
/// leaves the serial console fully usable.
pub(crate) fn install() {
    *FBCON.lock() = Console::try_new();
}

fn dimensions() -> Option<(usize, usize)> {
    fb::fbcon_dimensions().map(|(width, height)| {
        (
            (width / CELL_WIDTH).clamp(1, MAX_COLS),
            (height / CELL_HEIGHT).clamp(1, MAX_ROWS),
        )
    })
}

/// Writes terminal output to one virtual console.  Output is retained while
/// inactive so selecting that VT later reconstructs its screen.
pub(crate) fn write(vt: u16, bytes: &[u8], _active: u16, _graphics: bool) {
    let Some((cols, rows)) = dimensions() else {
        return;
    };
    {
        let mut console = FBCON.lock();
        let Some(console) = console.as_mut() else {
            return;
        };
        let Some(screen) = console.screens.get_mut(vt.saturating_sub(1) as usize) else {
            return;
        };
        for &byte in bytes {
            screen.put(byte, cols, rows);
        }
    }
    schedule_write_present(vt);
}

/// Presents from the write path.  Repaints are always deferred to the
/// trailing task: the write path only records the pending VT, so echo and
/// program output never block on MMIO-bound drawing.  The task repaints the
/// latest cells at most once per interval, and one final time after a burst
/// subsides.
fn schedule_write_present(vt: u16) {
    let mut spawn_trailing = false;
    {
        let mut state = PRESENT.lock();
        state.trailing_vt = Some(vt);
        if !state.trailing_running {
            state.trailing_running = true;
            spawn_trailing = true;
        }
    }
    if spawn_trailing
        && axtask::try_spawn_with_name(trailing_present, "fbcon-trailing".into()).is_err()
    {
        // Without the task the repaint would never run; paint now rather
        // than leaving stale cells on screen.
        let mut state = PRESENT.lock();
        state.trailing_running = false;
        state.trailing_vt = None;
        drop(state);
        present(vt, false);
    }
}

/// Repaints the coalesced state while writes keep arriving, then exits.  The
/// `PRESENT` lock alone serializes liveness with writers, so no write can
/// strand a pending repaint without a live task.
fn trailing_present() {
    loop {
        let wait = {
            let state = PRESENT.lock();
            let elapsed = axhal::time::monotonic_time().saturating_sub(state.last);
            WRITE_PRESENT_INTERVAL.saturating_sub(elapsed)
        };
        let _ = axtask::sleep(wait);
        let vt = {
            let mut state = PRESENT.lock();
            match state.trailing_vt.take() {
                Some(vt) => {
                    state.last = axhal::time::monotonic_time();
                    vt
                }
                None => {
                    state.trailing_running = false;
                    return;
                }
            }
        };
        present(vt, false);
    }
}

/// Mirrors `/dev/console` output to whichever VT is active, while its normal
/// writer continues to send the same bytes to the serial console.
pub(crate) fn write_active(bytes: &[u8], active: u16, graphics: bool) {
    write(active, bytes, active, graphics);
}

/// Repaints `vt` only after the VT arbiter has verified that it is active and
/// in KD_TEXT. The arbiter's sleeping presentation gate, not its state spin
/// lock, is held by the caller; no fbcon path re-enters that gate.
pub(crate) fn present_while_text_active(vt: u16) {
    let Some((cols, rows)) = dimensions() else {
        return;
    };
    fb::fbcon_draw(|frame| {
        frame.clear(0x0000_0000);
        // Snapshot one row at a time so writers are never excluded for the
        // MMIO-bound glyph drawing, only for a bounded cell copy.
        let mut row_cells = [b' '; MAX_COLS];
        for row in 0..rows {
            {
                let console = FBCON.lock();
                let Some(console) = console.as_ref() else {
                    return;
                };
                let Some(screen) = console.screens.get(vt.saturating_sub(1) as usize) else {
                    return;
                };
                row_cells[..cols]
                    .copy_from_slice(&screen.cells[row * MAX_COLS..row * MAX_COLS + cols]);
            }
            for (col, &byte) in row_cells[..cols].iter().enumerate() {
                frame.glyph(col * CELL_WIDTH, row * CELL_HEIGHT, byte);
            }
        }
    });
}

/// Revalidates a presentation request through the VT-owned serialization
/// protocol.  The caller-supplied graphics snapshot is deliberately ignored:
/// it may have become stale before this function runs.
pub(crate) fn present(vt: u16, _graphics: bool) {
    crate::pseudofs::dev::tty::VT_MANAGER.with_text_active(vt, || present_while_text_active(vt));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_wrap_and_scroll_without_allocating() {
        let mut screen = Screen::try_new().unwrap();
        screen.put(b'a', 2, 2);
        screen.put(b'b', 2, 2);
        screen.put(b'c', 2, 2);
        screen.put(b'd', 2, 2);
        screen.put(b'e', 2, 2);
        assert_eq!(screen.cells[0], b'c');
        assert_eq!(screen.cells[1], b'd');
        assert_eq!(screen.cells[MAX_COLS], b'e');
        assert_eq!(screen.cells[MAX_COLS + 1], b' ');
    }
}
