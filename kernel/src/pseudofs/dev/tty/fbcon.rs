//! Small, bounded framebuffer console for Linux virtual terminals.
//!
//! The serial console remains the authoritative early/debug console.  This
//! module mirrors terminal output into per-VT character cells once fbdev has
//! supplied a scanout surface.  The VT arbiter serializes the decision to
//! render with KD_GRAPHICS transitions; fbcon then holds the cell lock across
//! the bounded draw so a concurrent writer cannot make the presented cells
//! inconsistent with that decision.
//!
//! It is also where the kernel's own log reaches the screen.  Terminal output
//! arrives here because a process wrote to a console device; kernel `println!`
//! arrives through the klog ring instead, and on a machine with no serial port
//! the screen is the only console that log has.  See [`install_log_mirror`].

use alloc::{boxed::Box, vec::Vec};
use core::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use axerrno::AxError;
use axpoll::PollSet;
use axsync::Mutex;

use crate::{
    pseudofs::dev::{fb, tty::VT_MANAGER},
    readiness::block_on_poll_set_uninterruptible,
};

const VT_COUNT: usize = 63;
const MAX_COLS: usize = 160;
const MAX_ROWS: usize = 64;
const CELL_WIDTH: usize = 8;
const CELL_HEIGHT: usize = 16;

const DEFAULT_FG: u8 = 16;
const BOLD: u8 = 1;
const REVERSE: u8 = 2;
const PALETTE: [u32; 17] = [
    0x000000, 0xaa0000, 0x00aa00, 0xaa5500, 0x0000aa, 0xaa00aa, 0x00aaaa, 0xaaaaaa, 0x555555,
    0xff5555, 0x55ff55, 0xffff55, 0x5555ff, 0xff55ff, 0x55ffff, 0xffffff, 0xd0d0d0,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cell {
    byte: u8,
    fg: u8,
    bg: u8,
    attributes: u8,
}
impl Cell {
    const DEFAULT: Self = Self {
        byte: b' ',
        fg: DEFAULT_FG,
        bg: 0,
        attributes: 0,
    };
    fn colors(self) -> (u32, u32) {
        let foreground = if self.attributes & BOLD != 0 {
            match self.fg {
                0..=7 => self.fg + 8,
                DEFAULT_FG => 15,
                _ => self.fg,
            }
        } else {
            self.fg
        };
        let (fg, bg) = (PALETTE[foreground as usize], PALETTE[self.bg as usize]);
        if self.attributes & REVERSE != 0 {
            (bg, fg)
        } else {
            (fg, bg)
        }
    }
}

struct Screen {
    cells: Box<[Cell]>,
    row: usize,
    col: usize,
    style: Cell,
    saved: (usize, usize, Cell),
    scroll_top: usize,
    scroll_bottom: usize,
    wrap_pending: bool,
    autowrap: bool,
    cursor_visible: bool,
    escape: u8,
    params: [usize; 16],
    param: usize,
    private: bool,
    malformed: bool,
    utf8: [u8; 4],
    utf8_len: usize,
    utf8_need: usize,
}

impl Screen {
    fn try_new() -> Option<Self> {
        // Allocate on the heap: a Cell array is larger than a kernel stack.
        let mut cells = Vec::new();
        cells.try_reserve_exact(MAX_COLS * MAX_ROWS).ok()?;
        cells.resize(MAX_COLS * MAX_ROWS, Cell::DEFAULT);
        Some(Self {
            cells: cells.into_boxed_slice(),
            row: 0,
            col: 0,
            style: Cell::DEFAULT,
            saved: (0, 0, Cell::DEFAULT),
            scroll_top: 0,
            scroll_bottom: MAX_ROWS - 1,
            wrap_pending: false,
            autowrap: true,
            cursor_visible: true,
            escape: 0,
            params: [0; 16],
            param: 0,
            private: false,
            malformed: false,
            utf8: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
        })
    }

    fn clear(&mut self) {
        self.cells.fill(Cell::DEFAULT);
        self.row = 0;
        self.col = 0;
        self.style = Cell::DEFAULT;
        self.saved = (0, 0, Cell::DEFAULT);
        self.scroll_top = 0;
        self.scroll_bottom = MAX_ROWS - 1;
        self.wrap_pending = false;
        self.autowrap = true;
        self.cursor_visible = true;
        self.escape = 0;
        self.utf8_len = 0;
        self.utf8_need = 0;
    }

    fn scroll(&mut self, rows: usize, count: usize, down: bool) {
        let top = self.scroll_top.min(rows - 1);
        let bottom = self.scroll_bottom.min(rows - 1);
        if top > bottom {
            return;
        }
        let count = count.min(bottom - top + 1);
        let begin = top * MAX_COLS;
        let end = (bottom + 1) * MAX_COLS;
        let shift = count * MAX_COLS;
        if down {
            self.cells.copy_within(begin..end - shift, begin + shift);
            self.cells[begin..begin + shift].fill(self.style);
        } else {
            self.cells.copy_within(begin + shift..end, begin);
            self.cells[end - shift..end].fill(self.style);
        }
    }

    fn linefeed(&mut self, rows: usize) {
        if self.row == self.scroll_bottom.min(rows - 1) {
            self.scroll(rows, 1, false);
        } else {
            self.row = (self.row + 1).min(rows - 1);
        }
        self.wrap_pending = false;
    }

    fn restore(&mut self, cols: usize, rows: usize) {
        self.row = self.saved.0.min(rows - 1);
        self.col = self.saved.1.min(cols - 1);
        self.style = self.saved.2;
        self.wrap_pending = false;
    }

    fn csi(&mut self, byte: u8, cols: usize, rows: usize) {
        if self.malformed {
            return;
        }
        let n = self.params[0].max(1);
        if self.private {
            if matches!(byte, b'h' | b'l') {
                for index in 0..=self.param {
                    match self.params[index] {
                        7 => self.autowrap = byte == b'h',
                        25 => self.cursor_visible = byte == b'h',
                        _ => {}
                    }
                }
            }
            return;
        }
        match byte {
            b'H' | b'f' => {
                self.row = self.params[0].max(1).saturating_sub(1).min(rows - 1);
                self.col = self.params[1].max(1).saturating_sub(1).min(cols - 1);
            }
            b'A' => self.row = self.row.saturating_sub(n),
            b'B' | b'e' => self.row = self.row.saturating_add(n).min(rows - 1),
            b'C' | b'a' => self.col = self.col.saturating_add(n).min(cols - 1),
            b'D' => self.col = self.col.saturating_sub(n),
            b'E' => {
                self.row = self.row.saturating_add(n).min(rows - 1);
                self.col = 0;
            }
            b'F' => {
                self.row = self.row.saturating_sub(n);
                self.col = 0;
            }
            b'G' | b'`' => self.col = n.saturating_sub(1).min(cols - 1),
            b'd' => self.row = n.saturating_sub(1).min(rows - 1),
            b'K' => {
                let begin = self.row * MAX_COLS;
                let range = match self.params[0] {
                    0 => begin + self.col..begin + cols,
                    1 => begin..begin + self.col + 1,
                    2 => begin..begin + cols,
                    _ => return,
                };
                self.cells[range].fill(self.style);
            }
            b'J' => {
                let cursor = self.row * MAX_COLS + self.col;
                let range = match self.params[0] {
                    0 => cursor..rows * MAX_COLS,
                    1 => 0..cursor + 1,
                    2 => 0..rows * MAX_COLS,
                    _ => return,
                };
                self.cells[range].fill(self.style);
            }
            b'm' => {
                let mut index = 0;
                while index <= self.param {
                    match self.params[index] {
                        0 => self.style = Cell::DEFAULT,
                        1 => self.style.attributes |= BOLD,
                        7 => self.style.attributes |= REVERSE,
                        22 => self.style.attributes &= !BOLD,
                        27 => self.style.attributes &= !REVERSE,
                        30..=37 => self.style.fg = (self.params[index] - 30) as u8,
                        40..=47 => self.style.bg = (self.params[index] - 40) as u8,
                        90..=97 => self.style.fg = (self.params[index] - 90 + 8) as u8,
                        100..=107 => self.style.bg = (self.params[index] - 100 + 8) as u8,
                        39 => self.style.fg = DEFAULT_FG,
                        49 => self.style.bg = 0,
                        38 | 48 => {
                            index += match self.params.get(index + 1) {
                                Some(2) => 4,
                                Some(5) => 2,
                                _ => self.param,
                            };
                        }
                        _ => {}
                    }
                    index += 1;
                }
                return;
            }
            b'r' => {
                let top = n - 1;
                let bottom = if self.params[1] == 0 {
                    rows
                } else {
                    self.params[1]
                };
                if top < bottom.saturating_sub(1) && bottom <= rows {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom - 1;
                    self.row = 0;
                    self.col = 0;
                }
            }
            b'S' => self.scroll(rows, n, false),
            b'T' => self.scroll(rows, n, true),
            b'L' | b'M'
                if (self.scroll_top..=self.scroll_bottom.min(rows - 1)).contains(&self.row) =>
            {
                let top = self.scroll_top;
                self.scroll_top = self.row;
                self.scroll(rows, n, byte == b'L');
                self.scroll_top = top;
            }
            b'P' | b'@' | b'X' => {
                let begin = self.row * MAX_COLS + self.col;
                let end = self.row * MAX_COLS + cols;
                let count = n.min(cols - self.col);
                match byte {
                    b'P' => {
                        self.cells.copy_within(begin + count..end, begin);
                        self.cells[end - count..end].fill(self.style);
                    }
                    b'@' => {
                        self.cells.copy_within(begin..end - count, begin + count);
                        self.cells[begin..begin + count].fill(self.style);
                    }
                    _ => self.cells[begin..begin + count].fill(self.style),
                }
            }
            b's' => self.saved = (self.row, self.col, self.style),
            b'u' => self.restore(cols, rows),
            _ => return,
        }
        self.wrap_pending = false;
    }

    fn put(&mut self, byte: u8, cols: usize, rows: usize) {
        // Decode across write boundaries. The built-in font is ASCII-only;
        // render one visible fallback per non-ASCII scalar rather than silently
        // dropping text or interpreting continuation bytes as terminal controls.
        if self.utf8_len != 0 {
            if byte & 0xc0 == 0x80 {
                self.utf8[self.utf8_len] = byte;
                self.utf8_len += 1;
                if self.utf8_len == self.utf8_need {
                    self.utf8_len = 0;
                    self.put_byte(b'?', cols, rows);
                }
                return;
            }
            self.utf8_len = 0;
            self.put_byte(b'?', cols, rows);
        }
        if self.escape == 0 && byte >= 0x80 {
            self.utf8_need = match byte {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => {
                    self.put_byte(b'?', cols, rows);
                    return;
                }
            };
            self.utf8[0] = byte;
            self.utf8_len = 1;
            return;
        }
        self.put_byte(byte, cols, rows);
    }

    fn put_byte(&mut self, byte: u8, cols: usize, rows: usize) {
        let cols = cols.clamp(1, MAX_COLS);
        let rows = rows.clamp(1, MAX_ROWS);
        self.row = self.row.min(rows - 1);
        self.col = self.col.min(cols - 1);
        // OSC titles must not leak their payload into the text screen.
        if self.escape == 3 || self.escape == 4 {
            self.escape = if byte == 7 || (self.escape == 4 && byte == b'\\') {
                0
            } else if byte == 0x1b {
                4
            } else {
                3
            };
            return;
        }
        if matches!(byte, 0x18 | 0x1a) {
            self.escape = 0;
            return;
        }
        if byte == 0x1b {
            self.escape = 1;
            return;
        }
        match self.escape {
            1 => {
                self.escape = 0;
                match byte {
                    b'[' => {
                        self.escape = 2;
                        self.params.fill(0);
                        self.param = 0;
                        self.private = false;
                        self.malformed = false;
                    }
                    b']' => self.escape = 3,
                    b'(' | b')' => self.escape = 5,
                    b'7' => self.saved = (self.row, self.col, self.style),
                    b'8' => self.restore(cols, rows),
                    b'D' => self.linefeed(rows),
                    b'E' => {
                        self.col = 0;
                        self.linefeed(rows);
                    }
                    b'M' => {
                        if self.row == self.scroll_top {
                            self.scroll(rows, 1, true);
                        } else {
                            self.row = self.row.saturating_sub(1);
                        }
                        self.wrap_pending = false;
                    }
                    b'c' => self.clear(),
                    _ => {}
                }
                return;
            }
            2 => {
                match byte {
                    b'0'..=b'9' => {
                        self.params[self.param] = self.params[self.param]
                            .saturating_mul(10)
                            .saturating_add((byte - b'0') as usize)
                    }
                    b';' if self.param + 1 < self.params.len() => self.param += 1,
                    b'?' if self.param == 0 && self.params[0] == 0 => self.private = true,
                    0x40..=0x7e => {
                        self.escape = 0;
                        self.csi(byte, cols, rows);
                    }
                    _ => self.malformed = true,
                }
                return;
            }
            5 => {
                self.escape = 0;
                return;
            }
            _ => {}
        }
        match byte {
            b'\r' => {
                self.col = 0;
                self.wrap_pending = false;
            }
            b'\n' => self.linefeed(rows),
            8 => {
                self.col = self.col.saturating_sub(1);
                self.wrap_pending = false;
            }
            b'\t' => {
                self.col = ((self.col + 8) & !7).min(cols - 1);
                self.wrap_pending = false;
            }
            0x20..=0x7e => {
                if self.wrap_pending && self.autowrap {
                    self.col = 0;
                    self.linefeed(rows);
                }
                self.cells[self.row * MAX_COLS + self.col] = Cell { byte, ..self.style };
                if self.col + 1 == cols {
                    self.wrap_pending = true;
                } else {
                    self.col += 1;
                }
            }
            _ => {}
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
    let console = Console::try_new();
    let installed = console.is_some();
    *FBCON.lock() = console;
    if installed {
        if let Some((width, height)) = fb::fbcon_dimensions() {
            let size = console_window_size(width, height);
            // No fbcon, framebuffer or VT-state lock spans notification.
            // PTYs have independent emulator-supplied geometry, unchanged here.
            super::N_TTY.resize_window_size(size);
            for vt in 1..=VT_COUNT as u16 {
                VT_MANAGER.tty_for(vt).resize_window_size(size);
            }
        }
        install_log_mirror();
    }
}

/// Bytes of kernel log copied into the cell grid per pass.
///
/// The ring retains up to 64 KiB, so the first pass after installation is a
/// replay of the whole retained boot log.  Reading it in bounded chunks keeps
/// one pass from holding the console lock while it walks the entire ring.
const LOG_MIRROR_CHUNK: usize = 512;

/// Chunks one pass copies before returning to the worker loop.
///
/// Eight KiB is more than a screen can show but far less than the ring, so a
/// caught-up mirror still finishes the pass promptly and an overrun cannot
/// monopolise the worker.
const LOG_MIRROR_CHUNKS_PER_PASS: usize = 16;

/// How long a pass which exhausted its budget waits before reading again.
const LOG_MIRROR_SATURATED_INTERVAL: Duration = Duration::from_millis(33);

/// Wakes the kernel-log mirror when the klog ring has grown.
static LOG_MIRROR_WAKE: PollSet = PollSet::new();

/// Whether a pass is in progress, so two of them cannot share one cursor.
static LOG_MIRROR_WRITING: AtomicBool = AtomicBool::new(false);

/// Whether the mirror task exists, so a second [`install`] cannot start a
/// second copy of it.
static LOG_MIRROR_STARTED: AtomicBool = AtomicBool::new(false);

/// Wakes the kernel-log mirror from a scheduler safe point.
///
/// A log producer may hold the console lock or run in interrupt context and
/// must never wake a task itself, so it publishes an edge that the deferred
/// work dispatcher turns into this call.
pub(crate) fn notify_log_mirror() {
    LOG_MIRROR_WAKE.wake();
}

/// Mirrors the kernel log into the active virtual console.
///
/// The ring is read through a cursor rather than through the diagnostic
/// record queue, which makes the screen independent of the serial sink in
/// both directions: a machine with no serial port still shows the log, and a
/// serial port which has stopped accepting bytes cannot hold the screen back.
/// Starting the cursor at zero replays every retained byte, so the messages
/// printed before this console existed -- everything a serial-less machine
/// sent to a UART that is not there -- appear on the screen rather than being
/// lost.  Only bytes the ring has already overwritten are missed.
///
/// The cost of that independence is that the screen shows every retained
/// byte, not only the records the console threshold would have admitted.  The
/// ring stores no per-record level, so a filtered mirror is not possible from
/// here; and on a machine whose only console is the screen, filtering it is
/// what makes a failed boot undiagnosable.  Console level control therefore
/// keeps its existing meaning for the serial sink alone.
pub(crate) fn install_log_mirror() {
    if LOG_MIRROR_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if axtask::try_spawn_with_name(log_mirror_worker, "fbcon-log".into()).is_err() {
        // The screen still works for whatever userspace writes to it; only the
        // kernel's own log would be missing, so report and carry on.
        LOG_MIRROR_STARTED.store(false, Ordering::Release);
        warn!("Failed to start the kernel-log console mirror");
    }
}

/// Copies the kernel log into the active virtual console until it is drained,
/// then sleeps until a producer publishes more.
///
/// Writing into the cells of a VT which is showing graphics is deliberate: the
/// text belongs to that console, and a client which later yields the seat
/// repaints from these cells.  Dropping the bytes instead would leave a hole
/// in the log that returning to text could not fill.
fn log_mirror_worker() {
    let mut cursor = 0u64;
    loop {
        if mirror_new_log_bytes(&mut cursor) {
            // The ring grew at least as fast as this pass could read it, so
            // the wake below -- which is level-triggered on a non-empty ring
            // -- would return at once and the worker would spin.  Nothing
            // legitimate produces log faster than a console can show it, so
            // pace the pass instead of waiting on an edge that is already
            // set; a console which is somehow logging climbs at a reading
            // pace rather than pinning a CPU.
            let _ = axtask::sleep(LOG_MIRROR_SATURATED_INTERVAL);
            continue;
        }
        // Re-check after registering, so a record appended between the drain
        // above and this wait cannot leave the mirror asleep with work to do.
        if let Err(error) = block_on_poll_set_uninterruptible(&LOG_MIRROR_WAKE, || {
            if axruntime::klog::available_from(cursor) != 0 {
                Ok(())
            } else {
                Err(AxError::WouldBlock)
            }
        }) {
            warn!("Kernel-log console mirror stopped: {error}");
            return;
        }
    }
}

/// Copies everything the ring has retained since `cursor` into the console.
///
/// **Nothing on this path may log.**  The mirror is a reader of the ring it
/// writes the console from, so a record produced here would be read back and
/// written again: one log line would become an endless stream of them, and a
/// machine with no serial port would look hung behind a screen scrolling
/// faster than it can be read.  `the_console_write_path_does_not_log` in this
/// module's tests is what holds that down; the guard below only keeps a second
/// caller from racing this one over the same cursor.
fn mirror_new_log_bytes(cursor: &mut u64) -> bool {
    if LOG_MIRROR_WRITING.swap(true, Ordering::AcqRel) {
        return false;
    }
    let mut bytes = [0u8; LOG_MIRROR_CHUNK];
    let mut chunks = 0;
    while chunks < LOG_MIRROR_CHUNKS_PER_PASS {
        let (count, next) = axruntime::klog::snapshot_into(*cursor, &mut bytes, false);
        if count == 0 {
            break;
        }
        *cursor = next;
        // Kernel logs bypass termios, so apply their CRLF presentation here,
        // not in the VT parser used by raw-mode applications.
        let mut start = 0;
        for (index, byte) in bytes[..count].iter().enumerate() {
            if *byte == b'\n' {
                write_active(&bytes[start..index]);
                write_active(b"\r\n");
                start = index + 1;
            }
        }
        write_active(&bytes[start..count]);
        chunks += 1;
    }
    LOG_MIRROR_WRITING.store(false, Ordering::Release);
    chunks == LOG_MIRROR_CHUNKS_PER_PASS
}

fn console_window_size(width: usize, height: usize) -> super::terminal::WindowSize {
    super::terminal::WindowSize {
        ws_col: (width / CELL_WIDTH).clamp(1, MAX_COLS) as u16,
        ws_row: (height / CELL_HEIGHT).clamp(1, MAX_ROWS) as u16,
        ws_xpixel: width.min(u16::MAX as usize) as u16,
        ws_ypixel: height.min(u16::MAX as usize) as u16,
    }
}

fn dimensions() -> Option<(usize, usize)> {
    fb::fbcon_dimensions().map(|(width, height)| {
        let size = console_window_size(width, height);
        (usize::from(size.ws_col), usize::from(size.ws_row))
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

/// Mirrors output from a source which has no virtual console of its own --
/// the kernel log -- to whichever VT is active.  A source with a console of
/// its own writes to that console through [`write`] instead, so that output
/// meant for an inactive `ttyN` is not shown on the active one.
pub(crate) fn write_active(bytes: &[u8]) {
    let active = VT_MANAGER.active();
    write(active, bytes, active, VT_MANAGER.graphics(active));
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
        let mut row_cells = [Cell::DEFAULT; MAX_COLS];
        for row in 0..rows {
            let cursor_col = {
                let console = FBCON.lock();
                let Some(console) = console.as_ref() else {
                    return;
                };
                let Some(screen) = console.screens.get(vt.saturating_sub(1) as usize) else {
                    return;
                };
                row_cells[..cols]
                    .copy_from_slice(&screen.cells[row * MAX_COLS..row * MAX_COLS + cols]);
                (screen.cursor_visible && screen.row == row).then_some(screen.col)
            };
            for (col, &cell) in row_cells[..cols].iter().enumerate() {
                let (fg, bg) = cell.colors();
                frame.glyph(
                    col * CELL_WIDTH,
                    row * CELL_HEIGHT,
                    cell.byte,
                    fg,
                    bg,
                    cursor_col == Some(col),
                );
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

    /// A logger which forwards to the klog ring.
    ///
    /// Host tests install no kernel logger, so `info!` and `warn!` are
    /// silently dropped and a test that watched the ring would pass by being
    /// deaf.  This connects the same `log` facade the kernel uses to the ring
    /// the mirror reads, so the test below can see what the console logged.
    struct KlogTestLogger;

    impl log::Log for KlogTestLogger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            use core::fmt::Write as _;
            let mut text = alloc::string::String::new();
            let _ = write!(&mut text, "{}", record.args());
            axruntime::klog::record(text.as_bytes());
        }

        fn flush(&self) {}
    }

    static KLOG_TEST_LOGGER: KlogTestLogger = KlogTestLogger;

    /// Bytes the klog ring has ever held, which only ever grows.
    fn ring_end() -> u64 {
        axruntime::klog::snapshot_into(0, &mut [], true).1
    }

    #[test]
    fn raw_lf_preserves_column_and_utf8_survives_split_writes() {
        let mut screen = Screen::try_new().unwrap();
        feed(&mut screen, b"ab\nC", 8, 4);
        assert_eq!((screen.row, screen.col), (1, 3));
        assert_eq!(screen.cells[MAX_COLS + 2].byte, b'C');
        feed(&mut screen, &[0xe4, 0xb8], 8, 4);
        assert_eq!(screen.col, 3);
        feed(&mut screen, &[0xad], 8, 4);
        assert_eq!(screen.cells[MAX_COLS + 3].byte, b'?');
        feed(&mut screen, &[0xc2, 0x1b, b'[', b'2', b'J'], 8, 4);
        assert!(screen.cells.iter().all(|cell| cell.byte == b' '));
    }

    #[test]
    fn a_pass_is_bounded_and_reports_that_it_was() {
        // The mirror reads a ring it also writes the console from.  If
        // anything on that path ever logs, the ring grows at least as fast as
        // the mirror reads it and a pass that only stopped when the ring was
        // empty would never return.  Bounding the pass is what turns that into
        // a paced worker instead of a spin, and the saturated flag is what
        // tells the worker to pace itself.
        let mut cursor = ring_end();
        // More than one pass can carry, so the pass must stop at its budget.
        for line in 0..(LOG_MIRROR_CHUNK * (LOG_MIRROR_CHUNKS_PER_PASS + 4) / 24) {
            axruntime::klog::record(alloc::format!("saturate the ring {line:08}\n").as_bytes());
        }
        assert!(
            axruntime::klog::available_from(cursor) > LOG_MIRROR_CHUNK * LOG_MIRROR_CHUNKS_PER_PASS,
            "the ring did not hold more than one pass"
        );

        assert!(
            mirror_new_log_bytes(&mut cursor),
            "a full pass must report that it was saturated"
        );

        // Once drained the flag clears, so the worker goes back to sleeping on
        // the wake edge rather than polling.
        let mut drained = ring_end();
        assert!(!mirror_new_log_bytes(&mut drained));
    }

    #[test]
    fn the_console_write_path_does_not_log() {
        // The kernel-log mirror reads the ring this module writes into, so a
        // record produced here would be read back and written again: one log
        // line would become an endless stream of them, and on a machine with
        // no serial port that looks exactly like a hung kernel behind a screen
        // scrolling faster than it can be read.  This test is what keeps the
        // mirror's cursor safe to advance.
        let _context = crate::test_support::scheduler_test_context();
        // A previously installed logger wins; both forward to the ring, and
        // the canary below proves the forwarding works either way.
        let _ = log::set_logger(&KLOG_TEST_LOGGER);
        log::set_max_level(log::LevelFilter::Trace);
        // Installation may itself report a failure to spawn on the host; only
        // the console path below is under test.
        install();

        let before = ring_end();
        log::info!("canary");
        assert!(
            ring_end() > before,
            "the test logger is not connected to klog, so this test is deaf"
        );

        let before = ring_end();
        write(1, b"terminal output\n", 1, false);
        write_active(b"mirrored kernel log\n");
        present(1, false);
        present_while_text_active(1);

        assert_eq!(
            ring_end(),
            before,
            "the console write path appended to klog"
        );
    }

    fn feed(screen: &mut Screen, bytes: &[u8], cols: usize, rows: usize) {
        for &byte in bytes {
            screen.put(byte, cols, rows);
        }
    }

    #[test]
    fn vi_bottom_command_replaces_reverse_status_without_stale_text() {
        let mut screen = Screen::try_new().unwrap();
        feed(
            &mut screen,
            b"file contents\x1b[24;1H\x1b[7mfile 1L, 5C\x1b[0m",
            80,
            24,
        );
        let bottom = 23 * MAX_COLS;
        assert_eq!(screen.cells[bottom].colors(), (0, 0xd0d0d0));
        // Deliberately split a CUP between writes, as vi's stdio may do.
        feed(&mut screen, b"\x1b[24;", 80, 24);
        feed(&mut screen, b"1H\x1b[K:q", 80, 24);
        assert_eq!(screen.cells[bottom].byte, b':');
        assert_eq!(screen.cells[bottom + 1].byte, b'q');
        assert_eq!(screen.cells[bottom].colors(), (0xd0d0d0, 0));
        assert!(
            screen.cells[bottom + 2..bottom + 80]
                .iter()
                .all(|cell| cell.byte == b' ')
        );
        assert_eq!(screen.cells[0].byte, b'f');
        feed(&mut screen, b"\r\x1b[2K", 80, 24);
        assert!(
            screen.cells[bottom..bottom + 80]
                .iter()
                .all(|cell| *cell == Cell::DEFAULT)
        );
    }

    #[test]
    fn colors_save_restore_and_erase_do_not_move_the_cursor() {
        let mut screen = Screen::try_new().unwrap();
        feed(
            &mut screen,
            b"\x1b[2;3H\x1b[31;44;1m\x1b7\x1b[0m\x1b[Hn\x1b8R",
            8,
            4,
        );
        let cell = screen.cells[MAX_COLS + 2];
        assert_eq!(cell.byte, b'R');
        assert_eq!(cell.colors(), (0xff5555, 0x0000aa));
        feed(
            &mut screen,
            b"\x1b[0m\x1b[38;5;31m\x1b[38;2;1;2;3m\x1b[48;2;0;0;31m",
            8,
            4,
        );
        assert_eq!(screen.style, Cell::DEFAULT);
        feed(&mut screen, b"\x1b[0m\x1b[2J", 8, 4);
        assert_eq!((screen.row, screen.col), (1, 3));
        assert!(
            screen.cells[..4 * MAX_COLS]
                .iter()
                .all(|cell| *cell == Cell::DEFAULT)
        );
    }

    #[test]
    fn wrap_is_deferred_and_scroll_region_preserves_the_status_line() {
        let mut screen = Screen::try_new().unwrap();
        feed(&mut screen, b"ab\r\n", 2, 2);
        assert_eq!(screen.cells[0].byte, b'a');
        assert_eq!((screen.row, screen.col), (1, 0));
        screen.clear();
        feed(
            &mut screen,
            b"A\x1b[2;1HB\x1b[3;1HC\x1b[4;1HD\x1b[1;3r\x1b[3;1H\n",
            8,
            4,
        );
        assert_eq!(screen.cells[0].byte, b'B');
        assert_eq!(screen.cells[MAX_COLS].byte, b'C');
        assert_eq!(screen.cells[2 * MAX_COLS].byte, b' ');
        assert_eq!(screen.cells[3 * MAX_COLS].byte, b'D');
    }

    #[test]
    fn malformed_and_overlong_csi_are_bounded_even_on_one_cell_geometry() {
        let mut screen = Screen::try_new().unwrap();
        feed(&mut screen, b"\x1b[", 1, 1);
        for _ in 0..512 {
            screen.put(b'9', 1, 1);
        }
        feed(&mut screen, b";999999999999999999999999H!", 1, 1);
        assert_eq!((screen.row, screen.col), (0, 0));
        assert_eq!(screen.cells[0].byte, b'!');
        feed(
            &mut screen,
            b"\x1b[1;2;3;4;5;6;7;8;9;0;1;2;3;4;5;6;7;8m",
            1,
            1,
        );
        assert_eq!(screen.style, Cell::DEFAULT);
        feed(
            &mut screen,
            b"\x1b[999999999999999999999S\x1b[999999999999999999999T",
            1,
            1,
        );
        feed(&mut screen, b"\x1b]ignored title\x07z", 1, 1);
        assert_eq!(screen.cells[0].byte, b'z');
        feed(&mut screen, b"\x1b[999999;999999H", usize::MAX, usize::MAX);
        assert_eq!((screen.row, screen.col), (MAX_ROWS - 1, MAX_COLS - 1));
    }

    #[test]
    fn cursor_is_visible_by_default_and_obeys_deferred_private_mode_updates() {
        let mut screen = Screen::try_new().unwrap();
        assert!(screen.cursor_visible);
        feed(&mut screen, b"\x1b[?25", 80, 24);
        assert!(screen.cursor_visible);
        feed(&mut screen, b"l", 80, 24);
        assert!(!screen.cursor_visible);
        feed(&mut screen, b"\x1b[24;1H:q\x1b[?25h", 80, 24);
        assert!(screen.cursor_visible);
        assert_eq!((screen.row, screen.col), (23, 2));
    }

    #[test]
    fn physical_console_reports_the_same_bounded_grid_it_renders() {
        let size = console_window_size(1024, 768);
        assert_eq!((size.ws_col, size.ws_row), (128, 48));
        let size = console_window_size(usize::MAX, usize::MAX);
        assert_eq!(
            (usize::from(size.ws_col), usize::from(size.ws_row)),
            (MAX_COLS, MAX_ROWS)
        );
        let size = console_window_size(0, 0);
        assert_eq!((size.ws_col, size.ws_row), (1, 1));
    }

    #[test]
    fn cells_wrap_and_scroll_without_allocating() {
        let mut screen = Screen::try_new().unwrap();
        screen.put(b'a', 2, 2);
        screen.put(b'b', 2, 2);
        screen.put(b'c', 2, 2);
        screen.put(b'd', 2, 2);
        screen.put(b'e', 2, 2);
        assert_eq!(screen.cells[0].byte, b'c');
        assert_eq!(screen.cells[1].byte, b'd');
        assert_eq!(screen.cells[MAX_COLS].byte, b'e');
        assert_eq!(screen.cells[MAX_COLS + 1].byte, b' ');
    }
}
