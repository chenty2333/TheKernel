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
    let console = Console::try_new();
    let installed = console.is_some();
    *FBCON.lock() = console;
    if installed {
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
        write_active(&bytes[..count]);
        chunks += 1;
    }
    LOG_MIRROR_WRITING.store(false, Ordering::Release);
    chunks == LOG_MIRROR_CHUNKS_PER_PASS
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
