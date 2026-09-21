//! The kernel log: one producer path, a severity scale, and N consoles.
//!
//! Three axes decide what happens to a message, and only two of them belong to
//! the author of the call site:
//!
//! * **severity** -- a `log` level (`error`..`trace`) plus, when the message
//!   needs one of the priorities `log` cannot name, an explicit `KERN_SOH`
//!   priority prefix (§"Priorities"). Both land in the record's 3-bit priority.
//! * **target** -- the module path the `log` macros supply. It selects the
//!   emission cap, never a destination.
//! * **destination** -- decided by `console_loglevel` when a console reads the
//!   record, never by the call site. Producers do not choose a sink, and no
//!   message is written twice because an author wanted it on two sinks.
//!
//! There are two decisions and they are deliberately different in strength.
//! **Retention** is unconditional for `error`, `warn` and `info`: those records
//! enter the ring whether or not anything will print them, so asking a question
//! later (`dmesg`, `syslog(2)`) can answer it. Only the `debug`/`trace` band is
//! capped, per target, because that band is the one whose volume is a function
//! of how busy the kernel is. **Printing** is what the console level governs,
//! and a console that is quiet loses nothing -- the console is a reader of the
//! retained ring, holding a cursor rather than a copy of each record.
//!
//! Retention being unconditional has a price, and the rate limits pay it. A
//! record the debug band has asked for is bounded twice over: once per call
//! site, so one chatty place cannot spend the ring, and once globally, so a
//! thousand places each staying inside their own budget cannot either. What a
//! window refuses is counted, and the record that reopens the window says how
//! many were refused -- a suppression that leaves no trace is indistinguishable
//! from a kernel that never said it. The limits speak only to the debug band,
//! because that is the band whose volume follows how busy the kernel is: an
//! `error`, `warn` or `info` record never waits on a budget, and neither does
//! the machine's own death report, which is what priorities 0..2 are for.
//!
//! The ring is the log; every reader -- `syslog(2)`, the framebuffer console,
//! the boot screen, and the serial diagnostic console -- reads it through its
//! own cursor. That is what makes the two decisions independent: a burst a slow
//! console cannot keep up with costs nothing until the ring itself overwrites
//! text the console had not secured, and `log_stats` reports that.
//!
//! # Priorities
//!
//! `log` names five severities; the record and every console speak the eight
//! Linux syslog priorities, so the mapping is stated rather than implied:
//!
//! ```text
//!   0 emerg  \
//!   1 alert   }  `diagnostic_at`, or a `\x01N` prefix on any `log` record
//!   2 crit   /                        -- `log` has no name below error
//!   3 error   `error!`
//!   4 warn    `warn!`
//!   5 notice  `diagnostic()`'s own level, or `\x015`: `info!` means
//!                                       "informational", not "a normal but
//!                                       significant condition"
//!   6 info    `info!`
//!   7 debug   `debug!`, `trace!`      the two are one priority, as in Linux
//! ```
//!
//! Priorities 0..2 are reachable on purpose: a console level must be able to
//! mute routine output without muting the machine's own death report, and the
//! way to express that is a priority below any level a human would set.
//!
//! A prefix says how severe a message is, not how often it may be produced.
//! `debug!("\x010…")` is stored at priority 0 and rate limited as `debug`, which
//! is the intended reading: a call site that can be reached once per packet must
//! not be able to buy an unlimited run of the ring by claiming to be the panic it
//! is describing.
use core::{
    fmt::{self, Write},
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

use kspin::SpinNoIrq;
use log::{Level, LevelFilter, Log, Metadata, Record};

/// The retained log. Sized from the measured boot, not from a wish: a whole boot
/// is about 114 records and 25 KiB of text (`docs/design/kernel-log-retention.md`
/// §4), so this holds roughly ten boots before the front starts overwriting --
/// enough that a `dmesg` asked for after a long-running storm still finds the
/// boot that started it.
///
/// The ring is `bytes` plus [`Store::marks`], so this number is paid for twice
/// and it is paid for in `.data`: [`Store::new`] gives `console_loglevel` its
/// default and the serial console its `supported` assumption, and an object with
/// any non-zero initial byte cannot join the zero-filled `.bss` beside it.  That
/// is a copy of this much text at every load, which is what the loader does with
/// `.data`, and it is the price of the two defaults being load-bearing rather
/// than incidental: a record emitted before `init` must still be counted against
/// the console that will not get it, and a boot whose command line says nothing
/// prints at `CONSOLE_LOGLEVEL_DEFAULT`.
pub const CAPACITY: usize = 256 * 1024;
const RECORD_BYTES: usize = 1024;
/// The `<6>` a record's text starts with.
///
/// [`Text::with_reserved_leader`] leaves exactly this much room at the front, so
/// a `log` record can be formatted first and get its priority leader
/// afterwards -- the priority is not known until the body exists, because a
/// `\x01N` prefix is part of that body (§"Priorities" above).
const LEADER_BYTES: usize = 3;

/// The syslog priorities the record and every console speak, named because the
/// mapping in [`priority`] is a table and the code below compares against
/// entries of it. Numbers go the other way from `log` levels: a lower priority
/// is the more severe one, and a console prints a record whose priority is
/// *below* its level. `log` has no name below `error`, so 0 (`KERN_EMERG`) and 1
/// (`KERN_ALERT`) are reachable only through [`diagnostic_at`], [`fatal`] or a
/// `\x01N` prefix, and nothing compares against them: the budget speaks to the
/// debug band, and a console's level is a range rather than a per-priority rule.
const EMERG: u8 = 0;
const ERROR: u8 = 3;
const WARNING: u8 = 4;
const NOTICE: u8 = 5;
const INFO: u8 = 6;
const DEBUG: u8 = 7;

const FILTERS: usize = 16;
const PREFIX_BYTES: usize = 64;
/// Set on the first byte of a record in [`Store::marks`].
const RECORD_START: u8 = 0x80;
/// The record's console priority, in the low bits of a [`Store::marks`] byte.
const PRIORITY_MASK: u8 = 0x07;

/// `KERN_SOH`: the byte that introduces an in-message priority.
const SOH: u8 = 0x01;
/// A `\x01N` prefix is two bytes, and the whole message may carry several; this
/// bounds how far the parser will walk before it gives up and keeps the text.
const SOH_HEADERS: usize = 4;

/// `CONSOLE_LOGLEVEL_DEFAULT`: print everything up to and including `info`, so
/// `KERN_DEBUG` needs a word from the operator to reach a screen.
const CONSOLE_LOGLEVEL_DEFAULT: u8 = 7;
/// `CONSOLE_LOGLEVEL_QUIET`: what `quiet` sets.
const CONSOLE_LOGLEVEL_QUIET: u8 = 4;
/// `CONSOLE_LOGLEVEL_MIN`: the floor `syslog(2)` will not let you set below.
const CONSOLE_LOGLEVEL_MIN: u8 = 1;
/// `CONSOLE_LOGLEVEL_DEBUG`: what the `debug` boot parameter sets.  Above the
/// highest priority there is nothing left to admit, which is the point: it says
/// "print everything, with no exception for the debug band".
const CONSOLE_LOGLEVEL_DEBUG: u8 = 10;
/// `MESSAGE_LOGLEVEL_DEFAULT`, reported by `/proc/sys/kernel/printk`. `log`
/// always names a level, so nothing here applies it at store time.
const MESSAGE_LOGLEVEL_DEFAULT: u8 = 4;

/// `DEFAULT_RATELIMIT_INTERVAL`: the window a suppression counts over.
const DEFAULT_RATELIMIT_INTERVAL_MS: u64 = 5 * 1000;
/// `DEFAULT_RATELIMIT_BURST`: passes allowed per window before suppressing.
const DEFAULT_RATELIMIT_BURST: u32 = 10;
/// One slot per callsite is how Linux does this, through a
/// `DEFINE_RATELIMIT_STATE` in the calling function. Rust has no way to give an
/// arbitrary call site static storage, so callsites hash into this many slots;
/// two hot callsites that collide share a budget, which is the failure mode
/// worth naming rather than a per-callsite table costing one entry per `log`
/// call in the kernel.
const RATELIMIT_SLOTS: usize = 64;

#[derive(Clone, Copy)]
struct Text {
    bytes: [u8; RECORD_BYTES],
    /// Where this record's content starts. A `log` record formatted with
    /// [`Text::with_reserved_leader`] starts at [`LEADER_BYTES`] so its priority
    /// leader can be written afterwards; every other text starts at 0.
    head: usize,
    len: usize,
    truncated: bool,
}
impl Text {
    const fn new() -> Self {
        Self {
            bytes: [0; RECORD_BYTES],
            head: 0,
            len: 0,
            truncated: false,
        }
    }
    /// An empty record that leaves [`LEADER_BYTES`] at the front unused, so a
    /// producer can format the body first and write its `<N>` leader afterwards.
    const fn with_reserved_leader() -> Self {
        let mut text = Self::new();
        text.head = LEADER_BYTES;
        text.len = LEADER_BYTES;
        text
    }
    /// Write the priority leader the reservation made room for, closing the
    /// reservation so the record starts at byte zero.
    fn attach_leader(&mut self, priority: u8) {
        let digit = b'0' + (priority & PRIORITY_MASK);
        self.bytes[..LEADER_BYTES].copy_from_slice(&[b'<', digit, b'>']);
        self.head = 0;
    }
    /// Drop `count` bytes starting at `at`, closing the gap.
    fn remove(&mut self, at: usize, count: usize) {
        debug_assert!(at + count <= self.len);
        self.bytes.copy_within(at + count..self.len, at);
        self.len -= count;
    }
    fn finish(&mut self) {
        if self.truncated {
            const MARK: &[u8] = b" [truncated]";
            self.len = self.len.min(RECORD_BYTES - MARK.len() - 1);
            while self.len > 0 && self.bytes[self.len] & 0xc0 == 0x80 {
                self.len -= 1;
            }
            self.bytes[self.len..self.len + MARK.len()].copy_from_slice(MARK);
            self.len += MARK.len();
        }
        if self.len == self.head || self.bytes[self.len - 1] != b'\n' {
            self.bytes[self.len] = b'\n';
            self.len += 1;
        }
    }
    fn as_bytes(&self) -> &[u8] {
        &self.bytes[self.head..self.len]
    }
}
impl Write for Text {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut n = s.len().min(RECORD_BYTES - 1 - self.len);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.bytes[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        self.truncated |= n != s.len();
        Ok(())
    }
}
/// One record, copied out of the ring: what the console prints and the priority
/// its level filter compares against.
#[derive(Clone, Copy)]
struct Queued {
    text: Text,
    priority: u8,
}
/// Which of the two consoles a reader or a control belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleId {
    /// The serial port the boot probe selected for kernel logs.
    Serial,
    /// The framebuffer console the screen shows.
    Screen,
}
const CONSOLES: [ConsoleId; 2] = [ConsoleId::Serial, ConsoleId::Screen];

/// A console: a reader that shows the retained ring to a human.
///
/// What belongs to a console is its own existence and its own place in the ring.
/// The *level* does not: Linux has exactly one `console_loglevel`, and every
/// control that moves it -- `quiet`, `loglevel=`, `dmesg -n`, `
/// SYSLOG_ACTION_CONSOLE_*` -- moves it for all consoles at once, so this kernel
/// keeps one number too. Splitting the level per console would have invented a
/// knob no control surface can reach.
#[derive(Clone, Copy)]
struct Console {
    /// Can this console show anything at all?  A machine with no serial port has
    /// no serial console, and the screen has none until fbcon installs.
    supported: bool,
    /// A console whose worker has died cannot be re-armed: nothing would drain
    /// the work it enqueues.
    retired: bool,
    /// How far this console has secured the ring: every record starting before
    /// this offset has been printed or is held by the reader printing it.
    secured: u64,
    /// Records the ring overwrote before this console secured them: the only way
    /// a retained record can now fail to reach this console.
    lost: u64,
}
impl Console {
    /// A console with no reader is not owed a record: `Store::append` counts
    /// losses against consoles that could have shown them.
    const fn new(supported: bool) -> Self {
        Self {
            supported,
            retired: false,
            secured: 0,
            lost: 0,
        }
    }
    fn shows(&self) -> bool {
        self.supported && !self.retired
    }
}
/// The ring: `N` bytes of retained text and a mark beside each one.
///
/// `N` is a parameter so the host tests can build a ring they can copy and wrap
/// on a test thread's stack; the one the kernel uses is [`CAPACITY`].
struct Store<const N: usize> {
    bytes: [u8; N],
    /// The priority of the record each retained byte belongs to, with
    /// [`RECORD_START`] set on a record's first byte.  A record's text does not
    /// carry its priority -- a `diagnostic()` fragment is arbitrary bytes -- and
    /// the console's level filter needs it when the console reaches the record,
    /// so it is kept beside the text instead of in a second copy of the record.
    marks: [u8; N],
    oldest: u64,
    end: u64,
    /// `console_loglevel`: a record prints when its priority is below this.  The
    /// comparison is exclusive exactly as `level >= console_loglevel` suppresses
    /// in Linux's `suppress_message_printing()`, so `0` is a console that shows
    /// nothing and 8 is one that shows the debug band too.
    console_loglevel: u8,
    consoles: [Console; CONSOLES.len()],
}
impl<const N: usize> Store<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            marks: [0; N],
            oldest: 0,
            end: 0,
            console_loglevel: CONSOLE_LOGLEVEL_DEFAULT,
            // Whether a serial console exists at all is the platform's answer,
            // set from the probed port by `init`; assuming one until then is
            // what keeps a record emitted before `init` from being written off
            // as undeliverable.  The screen's reader announces itself when
            // fbcon installs, so a machine that never shows a console is not
            // counted as losing records to it.
            consoles: [Console::new(true), Console::new(false)],
        }
    }
    fn console(&self, id: ConsoleId) -> &Console {
        &self.consoles[id as usize]
    }
    fn console_mut(&mut self, id: ConsoleId) -> &mut Console {
        &mut self.consoles[id as usize]
    }
    /// Would this console show a record of this priority right now?
    fn prints(&self, id: ConsoleId, priority: u8) -> bool {
        self.console(id).shows() && priority < self.console_loglevel
    }
    /// Retire one console's reader.
    ///
    /// A console that prints nothing more cannot lose anything more either, so
    /// its cursor goes to the end with it: leaving it behind would count every
    /// later overwrite as a record this console never printed, which is a report
    /// about a switch someone flipped rather than about the ring.
    fn retire(&mut self, id: ConsoleId) {
        let end = self.end;
        let console = self.console_mut(id);
        console.retired = true;
        console.secured = end;
    }
    /// Retain one record.
    ///
    /// The ring is the log and the only copy of it, so this never refuses a
    /// record: each console reads it later through its own cursor, and a console
    /// that cannot keep up costs nothing until the ring itself overwrites what
    /// that console had not secured.
    fn append(&mut self, text: &Text, priority: u8) {
        let priority = priority & PRIORITY_MASK;
        for (index, byte) in text.as_bytes().iter().enumerate() {
            let slot = self.end as usize % N;
            // The byte this write destroys sits a whole ring behind the write
            // cursor, so the question "had the console secured it?" is about
            // that offset and not about `end`.  Comparing against `end` asks
            // whether the console is behind the *writer*, which is true almost
            // always, and counts every record the ring wraps over as lost even
            // when the console printed it long ago.
            let overwritten = self.end.saturating_sub(N as u64);
            // A record the level mutes is not owed to any console: the console
            // worker is woken only by records it would print, so a muted run can
            // sit unsecured until the ring wraps over it, and counting that as a
            // loss would report records nobody was going to show.  The level
            // asked is the one in force now, when the record is destroyed.
            let destroys_a_head = self.marks[slot] & RECORD_START != 0
                && self.marks[slot] & PRIORITY_MASK < self.console_loglevel;
            for console in &mut self.consoles {
                // Equality counts.  `secured` only advances past a record once
                // the console has copied it out, so a console equal to this
                // offset has not read the record whose head is here yet.
                //
                // A console that cannot show anything -- no port, or a reader
                // that has retired -- holds its cursor at the end, so it is
                // never owed a record: counting its absence as a loss would
                // report a machine losing logs to a device it never had.
                if destroys_a_head && console.shows() && console.secured <= overwritten {
                    // Count the record, not the event: `log_stats` reports
                    // records, and a reader must be able to tell "the ring
                    // wrapped" from "this console lost this many records".
                    console.lost += 1;
                }
            }
            self.bytes[slot] = *byte;
            self.marks[slot] = priority
                | if index == 0 {
                    // A record that starts at a retained offset is reachable
                    // even after an earlier wrap.
                    RECORD_START
                } else {
                    0
                };
            self.end += 1;
        }
        self.oldest = self.end.saturating_sub(N as u64);
    }
    fn snapshot(&self, cursor: u64, dst: &mut [u8], newest: bool) -> (usize, u64) {
        let mut start = cursor.max(self.oldest).min(self.end);
        let n = dst.len().min((self.end - start) as usize);
        if newest {
            start = self.end - n as u64;
        }
        for (i, byte) in dst[..n].iter_mut().enumerate() {
            *byte = self.bytes[(start as usize + i) % N];
        }
        (n, start + n as u64)
    }
    /// The first retained byte at or after `from` that starts a record.
    ///
    /// The oldest retained byte is not necessarily a record boundary: the ring
    /// wraps on a byte, not on a record.  A reader that has fallen behind must
    /// resume at a boundary, or it would print the tail of a record whose head
    /// the ring already overwrote as if it were a record of its own.
    fn record_start(&self, from: u64) -> u64 {
        let mut at = from.max(self.oldest).min(self.end);
        while at < self.end && self.marks[at as usize % N] & RECORD_START == 0 {
            at += 1;
        }
        at
    }
    /// The record that starts at `cursor`, and the cursor just past it.
    ///
    /// A cursor the ring has already passed is moved up to the next retained
    /// record: the records it skipped are counted lost by [`Store::append`].
    ///
    /// A record ends at the next record's start mark, or at [`Store::end`] for
    /// the newest record -- never at a newline.  `Text::finish` terminates every
    /// record with a newline, but the text inside one may contain newlines of
    /// its own (the igc driver's absence report is two lines in one `info!`), so
    /// a newline is data and only a mark delimits.  `RECORD_BYTES` is the bound
    /// the producer side enforces on one `Text`, so a reader that stops there
    /// stops at the end of a maximal record, not inside a shorter one.
    fn peek(&self, cursor: u64) -> Option<(Queued, u64)> {
        let mut at = self.record_start(cursor);
        if at >= self.end {
            return None;
        }
        let priority = self.marks[at as usize % N] & PRIORITY_MASK;
        let mut text = Text::new();
        while text.len < RECORD_BYTES {
            if at >= self.end {
                break;
            }
            // Once the first byte is copied, a start mark on the byte at `at`
            // belongs to the next record and is this record's end.
            if text.len > 0 && self.marks[at as usize % N] & RECORD_START != 0 {
                break;
            }
            text.bytes[text.len] = self.bytes[at as usize % N];
            text.len += 1;
            at += 1;
        }
        Some((Queued { text, priority }, at))
    }
}
static STORE: SpinNoIrq<Store<CAPACITY>> = SpinNoIrq::new(Store::new());
/// One coalesced arrival edge per console: set when a record that console would
/// print enters the ring, consumed by the task that drains it. Per console
/// rather than global because a machine with no serial port must not have its
/// screen woken by work the screen will never show, and a console that is
/// switched off must not be kept busy by records it is not printing.
static PENDING: [AtomicBool; CONSOLES.len()] = [const { AtomicBool::new(false) }; CONSOLES.len()];
static READERS_PENDING: AtomicBool = AtomicBool::new(false);
static LOST_RECORDS: AtomicU64 = AtomicU64::new(0);
static TRUNCATED: AtomicU64 = AtomicU64::new(0);
static PRODUCING: [AtomicBool; axconfig::plat::MAX_CPU_NUM] =
    [const { AtomicBool::new(false) }; axconfig::plat::MAX_CPU_NUM];
/// Records a producer on a CPU already inside the producer path handed to the
/// producer that owns that CPU, which publishes them on its way out.
static DEFERRED: SpinNoIrq<[Option<Queued>; axconfig::plat::MAX_CPU_NUM]> =
    SpinNoIrq::new([const { None }; axconfig::plat::MAX_CPU_NUM]);

#[derive(Clone, Copy)]
struct Override {
    prefix: [u8; PREFIX_BYTES],
    len: usize,
    level: LevelFilter,
}
impl Override {
    const EMPTY: Self = Self {
        prefix: [0; PREFIX_BYTES],
        len: 0,
        level: LevelFilter::Off,
    };
    /// Does this term name `target`?
    ///
    /// A term is a module path or a prefix of one, and it matches a whole path
    /// segment at a time: `tk_kernel::task` covers `tk_kernel::task::signal` but
    /// not `tk_kernel::task_tools`. A bare byte prefix would let a term reach
    /// into a word inside a segment, and then "every filter is a module path"
    /// stops being true -- the one property that makes a target worth writing
    /// down is that you can name what it selects.
    fn matches(&self, target: &str) -> bool {
        let bytes = target.as_bytes();
        if bytes.len() < self.len || bytes[..self.len] != self.prefix[..self.len] {
            return false;
        }
        // Either the whole target, or the next thing is a segment separator.
        bytes.len() == self.len
            || (bytes.len() > self.len + 1
                && bytes[self.len] == b':'
                && bytes[self.len + 1] == b':')
    }
}
#[derive(Clone, Copy)]
struct Filter {
    default: LevelFilter,
    entries: [Override; FILTERS],
    count: usize,
}
impl Filter {
    const fn new() -> Self {
        Self {
            default: LevelFilter::Info,
            entries: [Override::EMPTY; FILTERS],
            count: 0,
        }
    }
    fn level(&self, target: &str) -> LevelFilter {
        let mut selected = self.default;
        let mut longest = 0;
        for entry in &self.entries[..self.count] {
            if entry.len > longest && entry.matches(target) {
                longest = entry.len;
                selected = entry.level;
            }
        }
        selected
    }
    /// What `log::set_max_level` is kept at.
    ///
    /// This is the reason the emission decision is nearly free. `log`'s own gate
    /// is one relaxed atomic read at the call site, so a `debug!` in a hot path
    /// costs that read and nothing else whenever no filter term admits the debug
    /// band anywhere -- no lock, no formatting, no target comparison. Only once
    /// something asks for the debug band does the per-target decision get
    /// consulted. Linux does the same split with a static key per `pr_debug`
    /// callsite; this cannot give a call site static storage, so it lifts the
    /// coarse half of the decision into the one global the facade already reads.
    ///
    /// The gate never goes below `info`, because this kernel retains `error`,
    /// `warn` and `info` unconditionally: a filter of `off` closes the debug band
    /// and must not silence the log. Anything the facade lets through then meets
    /// [`emits`], which is where the target's own cap applies.
    fn facade_gate(&self) -> LevelFilter {
        let mut highest = self.default;
        for entry in &self.entries[..self.count] {
            if entry.level > highest {
                highest = entry.level;
            }
        }
        highest.max(LevelFilter::Info)
    }
    fn parse(text: &str) -> Result<Self, ()> {
        let mut parts = text.trim().split(',');
        let mut out = Self::new();
        out.default = parts.next().ok_or(())?.trim().parse().map_err(|_| ())?;
        for part in parts {
            let (prefix, level) = part.trim().split_once('=').ok_or(())?;
            if prefix.is_empty()
                || prefix.len() > PREFIX_BYTES
                || out.count == FILTERS
                || !prefix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-' | b'.'))
                || out.entries[..out.count]
                    .iter()
                    .any(|e| &e.prefix[..e.len] == prefix.as_bytes())
            {
                return Err(());
            }
            let mut entry = Override::EMPTY;
            entry.prefix[..prefix.len()].copy_from_slice(prefix.as_bytes());
            entry.len = prefix.len();
            entry.level = level.parse().map_err(|_| ())?;
            out.entries[out.count] = entry;
            out.count += 1;
        }
        Ok(out)
    }
    fn render(&self, out: &mut impl Write) -> fmt::Result {
        write!(out, "{}", level_name(self.default))?;
        for entry in &self.entries[..self.count] {
            write!(
                out,
                ",{}={}",
                // `parse` admits only ASCII alphanumerics and four punctuation
                // bytes, so this decodes for every prefix the filter can hold.
                // It is still `unwrap_or` rather than `unwrap`, as `record`
                // below is: this renders into a file, and a panic in the
                // kernel's own formatting path is a worse answer to a bad
                // prefix than printing that it is one.
                core::str::from_utf8(&entry.prefix[..entry.len])
                    .unwrap_or("[invalid filter prefix]"),
                level_name(entry.level)
            )?;
        }
        writeln!(out)
    }
}
fn level_name(level: LevelFilter) -> &'static str {
    match level {
        LevelFilter::Off => "off",
        LevelFilter::Error => "error",
        LevelFilter::Warn => "warn",
        LevelFilter::Info => "info",
        LevelFilter::Debug => "debug",
        LevelFilter::Trace => "trace",
    }
}
static FILTER: SpinNoIrq<Filter> = SpinNoIrq::new(Filter::new());

/// Set the emission cap. Replacement grammar: `info,target_prefix=debug,
/// longer_prefix=trace`. At most 16 unique ASCII prefixes, each at most 64
/// bytes. Longest prefix wins. Invalid input leaves the previous configuration
/// untouched; callers authorize writes.
///
/// The cap governs the `debug` band and nothing else. `error`, `warn` and `info`
/// always reach the ring, because keeping a record is not the same decision as
/// printing one: a message you were not willing to show at this severity may
/// still be the answer to a question asked later, and `dmesg` is where you ask
/// it. Writing `off` therefore means "no debug records", not "be silent" --
/// silence is a console decision, said by [`set_console_loglevel`] and by
/// `dmesg -n`, whose `SYSLOG_ACTION_CONSOLE_LEVEL` is what that function serves.
pub fn set_filter(text: &str) -> Result<(), ()> {
    let next = Filter::parse(text)?;
    let mut filter = FILTER.lock();
    *filter = next;
    // Publish the facade's cheap gate from the same snapshot, so the two gates
    // cannot disagree about what the debug band admits. Growing the cap a few
    // instructions before `max_level` follows loses a handful of debug records
    // to a decision nobody made yet; shrinking it later asks the filter and is
    // told no. Neither is a lost record that mattered.
    let gate = filter.facade_gate();
    drop(filter);
    log::set_max_level(gate);
    Ok(())
}
pub fn write_filter(out: &mut impl Write) -> fmt::Result {
    let snapshot = *FILTER.lock();
    snapshot.render(out)
}
/// The one console-level statement `/proc/sys/kernel/printk` exposes.
///
/// Four values, in Linux's order: `console_loglevel`,
/// `default_message_loglevel`, `minimum_console_loglevel`,
/// `default_console_loglevel`. The first is the live one; the rest are the
/// constants a reader of this file expects beside it -- `CONSOLE_ON` restores
/// neither, because what it restores is the level `CONSOLE_OFF` replaced, and
/// that one belongs to the syscall layer, which keeps it the way Linux's
/// `saved_console_loglevel` does.
pub fn write_printk(out: &mut impl Write) -> fmt::Result {
    writeln!(
        out,
        "\t{}\t{}\t{}\t{}",
        console_loglevel(),
        MESSAGE_LOGLEVEL_DEFAULT,
        CONSOLE_LOGLEVEL_MIN,
        CONSOLE_LOGLEVEL_DEFAULT
    )
}
pub fn write_stats(out: &mut impl Write) -> fmt::Result {
    let (overwritten, serial, screen, loglevel, rate_dropped) = {
        let store = STORE.lock();
        (
            store.oldest,
            *store.console(ConsoleId::Serial),
            *store.console(ConsoleId::Screen),
            store.console_loglevel,
            RATE_DROPPED.load(Ordering::Relaxed),
        )
    };
    writeln!(
        out,
        "records_dropped {}\ndiagnostic_records_dropped {}\nrecords_truncated \
         {}\nretention_bytes_overwritten {}\ndiagnostic_supported {}\ndiagnostic_retired \
         {}\nscreen_records_dropped {}\nconsole_loglevel {}\nmessages_suppressed \
         {}\nratelimit_interval_ms {}\nratelimit_burst {}",
        LOST_RECORDS.load(Ordering::Relaxed),
        serial.lost,
        TRUNCATED.load(Ordering::Relaxed),
        overwritten,
        serial.supported as u8,
        serial.retired as u8,
        screen.lost,
        loglevel,
        rate_dropped,
        ratelimit_interval_ms(),
        ratelimit_burst(),
    )
}
/// Does this record get made at all?
///
/// `error`, `warn` and `info` always do; only the debug band consults the filter.
/// The order matters for cost, not just semantics: `log`'s own `max_level()` read
/// has already rejected anything the whole kernel has not asked for, so the lock
/// below is taken only for a record some target actually enabled.
fn emits(level: Level, target: &str) -> bool {
    if level <= Level::Info {
        return true;
    }
    // Read the filter rather than refusing to read it.  The critical section is
    // a bounded scan of at most `FILTERS` prefixes, and a record thrown away
    // because another CPU was replacing the filter at that instant is a record
    // lost for no reason.
    level <= FILTER.lock().level(target)
}
fn priority(level: Level) -> u8 {
    match level {
        Level::Error => ERROR,
        Level::Warn => WARNING,
        Level::Info => INFO,
        Level::Debug | Level::Trace => DEBUG,
    }
}
/// The priority a `\x01N` prefix in a formatted body asks for, and how many
/// bytes of body it consumes.
///
/// `log` names five severities and the record speaks eight priorities, so the
/// three below `error` and the one between `info` and `warn` arrive the way
/// Linux sends them: as a `KERN_SOH` prefix on the message. Like Linux, the
/// prefix is parsed *after* formatting, because a level can reach the buffer as
/// an argument rather than as part of the literal (`printk.c:2300-2305`).
/// The first header wins, later ones are stripped but do not change the
/// priority, and a byte that is not a valid level ends the scan and stays in the
/// text -- an accidental `\x01` in a message must not silently rewrite its
/// severity.
fn parsed_priority(body: &[u8]) -> (Option<u8>, usize) {
    let mut skip = 0;
    let mut selected = None;
    for _ in 0..SOH_HEADERS {
        if body.len() < skip + 2 || body[skip] != SOH {
            break;
        }
        let digit = body[skip + 1];
        if !digit.is_ascii_digit() || digit - b'0' > PRIORITY_MASK {
            break;
        }
        let level = digit - b'0';
        if selected.is_none() {
            selected = Some(level);
        }
        skip += 2;
    }
    (selected, skip)
}
/// One window of one rate-limited callsite, or of the global storm guard.
#[derive(Clone, Copy)]
struct RateWindow {
    /// The millisecond the current window opened.  `0` is the boot: a window is
    /// opened by the first record that needs one, and `passed: 0` says nothing
    /// has been spent from it yet.
    since_ms: u64,
    passed: u32,
    /// Records this window refused, waiting to be reported when it reopens.
    missed: u32,
}
impl RateWindow {
    const IDLE: Self = Self {
        since_ms: 0,
        passed: 0,
        missed: 0,
    };
}
/// Per-callsite windows, hashed by callsite. A hot line that fires once per
/// packet gets its own budget, so one noisy driver cannot spend the allowance of
/// the rest of the kernel.
static RATE_WINDOWS: SpinNoIrq<[RateWindow; RATELIMIT_SLOTS]> =
    SpinNoIrq::new([RateWindow::IDLE; RATELIMIT_SLOTS]);
/// The storm guard: a budget over every callsite at once. Per-callsite windows
/// cannot bound total volume -- a thousand places each printing ten lines per
/// window is ten thousand lines -- and once retention is unconditional the ring is
/// the only thing standing between a storm and a boot that erased its own
/// evidence.
///
/// The number has to sit above an ordinary boot or it throttles the boot instead
/// of the storm: a whole boot is about 114 records
/// (`docs/design/kernel-log-retention.md` §4), so the guard opens at a bit over
/// two boots per window.  What it buys is a bound on the erasure rate: two
/// windows' worth of records is the worst a storm can erase before the guard
/// itself goes quiet, and at the ring's average record size that is a second of
/// full tilt rather than an unlimited scroll.
const STORM_BURST: u32 = 256;
static STORM: SpinNoIrq<RateWindow> = SpinNoIrq::new(RateWindow::IDLE);
static RATELIMIT_INTERVAL_MS: AtomicU64 = AtomicU64::new(DEFAULT_RATELIMIT_INTERVAL_MS);
static RATELIMIT_BURST: AtomicU32 = AtomicU32::new(DEFAULT_RATELIMIT_BURST);
/// Records refused by either guard, since boot.
static RATE_DROPPED: AtomicU64 = AtomicU64::new(0);

/// The rate-limit window, in milliseconds -- `printk_ratelimit` in Linux, which
/// counts in jiffies.  Named for what it is here because a reader of
/// `/proc/sys/kernel/printk_ratelimit_ms` should not have to convert.
pub fn ratelimit_interval_ms() -> u64 {
    RATELIMIT_INTERVAL_MS.load(Ordering::Relaxed)
}
/// Passes allowed per window per call site: `printk_ratelimit_burst`.
pub fn ratelimit_burst() -> u32 {
    RATELIMIT_BURST.load(Ordering::Relaxed)
}
/// `printk_ratelimit`, in jiffies on Linux and in milliseconds here.
pub fn set_ratelimit_interval_ms(value: u64) {
    RATELIMIT_INTERVAL_MS.store(value, Ordering::Relaxed);
}
pub fn set_ratelimit_burst(value: u32) {
    RATELIMIT_BURST.store(value, Ordering::Relaxed);
}

/// What one guard decided about a record at `now_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admit {
    /// Store it.
    Yes,
    /// Store it, and report what the closed window refused.
    YesWithNotice(u32),
    /// Do not store it; it has been counted.
    No,
}
impl RateWindow {
    /// The `___ratelimit()` decision: refill on a new window, pass until the
    /// burst is spent, count the refusals, and let the record that reopens the
    /// window carry the count of the one before it.
    fn decide(&mut self, now_ms: u64, window_ms: u64, allowance: u32) -> Admit {
        if now_ms.saturating_sub(self.since_ms) >= window_ms {
            let missed = self.missed;
            *self = RateWindow {
                since_ms: now_ms,
                passed: 1,
                missed: 0,
            };
            return if missed > 0 {
                Admit::YesWithNotice(missed)
            } else {
                Admit::Yes
            };
        }
        if self.passed < allowance {
            self.passed += 1;
            Admit::Yes
        } else {
            self.missed = self.missed.saturating_add(1);
            RATE_DROPPED.fetch_add(1, Ordering::Relaxed);
            Admit::No
        }
    }
}
/// The callsite's window slot. A `&'static str` file identity is the same
/// pointer for every call from the same place, so the hash is stable without
/// giving a call site static storage of its own.
fn rate_slot(file: Option<&str>, line: Option<u32>) -> usize {
    let file = file.map_or(0, |name| name.as_ptr() as usize);
    file.wrapping_mul(0x0100_0193) ^ line.unwrap_or(0) as usize
}
/// The rate-limit state is global, so a test that counts what a window refused
/// has to start from a known one.  Nothing in the kernel calls this: a window
/// that has never been opened is already `IDLE`, and the boot that opens one is
/// the boot whose records are being counted.
#[cfg(test)]
fn reset_rate_state() {
    *STORM.lock() = RateWindow::IDLE;
    *RATE_WINDOWS.lock() = [RateWindow::IDLE; RATELIMIT_SLOTS];
    RATE_DROPPED.store(0, Ordering::Relaxed);
}
/// Both guards, in order: the callsite first so one place's budget is its own,
/// then the storm guard so no set of places can exceed the ring's patience.
fn admits(file: Option<&str>, line: Option<u32>, now_ms: u64) -> Admit {
    let window = ratelimit_interval_ms();
    let per_callsite = {
        let mut windows = RATE_WINDOWS.lock();
        let slot = rate_slot(file, line) % RATELIMIT_SLOTS;
        windows[slot].decide(now_ms, window, ratelimit_burst())
    };
    if per_callsite == Admit::No {
        return per_callsite;
    }
    let storm = STORM.lock().decide(now_ms, window, STORM_BURST);
    match (per_callsite, storm) {
        // A callsite that just reopened reports its own losses; the storm
        // guard's count rides along with it rather than competing for the slot.
        (Admit::YesWithNotice(callsite), Admit::YesWithNotice(storm)) => {
            Admit::YesWithNotice(callsite.saturating_add(storm))
        }
        (Admit::YesWithNotice(callsite), Admit::Yes) => Admit::YesWithNotice(callsite),
        (Admit::Yes, Admit::YesWithNotice(storm)) => Admit::YesWithNotice(storm),
        (Admit::Yes, Admit::Yes) => Admit::Yes,
        // A record the callsite allowed and the storm guard refused is lost for a
        // reason the callsite's own counter never sees, which is why
        // `messages_suppressed` counts both guards rather than one.
        (_, Admit::No) => Admit::No,
        // The callsite's own refusal returned above, so the guard that would
        // have had to disagree with it never ran.  Naming the case keeps the
        // match total without inventing a decision this function cannot make.
        (Admit::No, _) => Admit::No,
    }
}
struct ProducerGuard {
    cpu: usize,
    // Rust drops fields after our Drop body: clear the per-CPU recursion bit
    // before reenabling preemption (which can schedule another logging task).
    _preempt: kernel_guard::NoPreempt,
}
impl Drop for ProducerGuard {
    fn drop(&mut self) {
        // Clear the bit first, before preemption comes back and can schedule
        // another logging task on this CPU.  A producer that started while it
        // was set handed its record over rather than taking the ring from us;
        // one that starts now publishes its own record, and publishes this one
        // too if it gets to the slot first.
        PRODUCING[self.cpu].store(false, Ordering::Release);
        let deferred = { DEFERRED.lock()[self.cpu].take() };
        if let Some(record) = deferred {
            append(&record.text, record.priority);
        }
    }
}
fn current_cpu() -> usize {
    #[cfg(not(test))]
    {
        axhal::percpu::this_cpu_id()
    }
    #[cfg(test)]
    {
        // The host tests have no CPU id.  One slot is enough for them: no test
        // logs from two threads at once, so a test that holds a lock in a
        // helper thread is testing contention, not recursion.
        0
    }
}
/// Hand a finished record to the ring.
///
/// A record produced while this CPU is already inside the producer path -- an
/// interrupt handler interrupting a log, or anything else the log path itself
/// reached -- cannot take the ring: the producer that owns this CPU holds it
/// across its own append, and the lock is not reentrant.  Hand the record to
/// that producer instead of dropping it; only a second hand-over in the same
/// window, which one slot per CPU cannot hold, is refused and counted.
fn publish(text: &Text, priority: u8) {
    let preempt = kernel_guard::NoPreempt::new();
    let cpu = current_cpu();
    if PRODUCING[cpu].swap(true, Ordering::Acquire) {
        let mut deferred = DEFERRED.lock();
        if deferred[cpu].is_none() {
            deferred[cpu] = Some(Queued {
                text: *text,
                priority,
            });
        } else {
            LOST_RECORDS.fetch_add(1, Ordering::Relaxed);
        }
        return;
    }
    let guard = ProducerGuard {
        cpu,
        _preempt: preempt,
    };
    append(text, priority);
    drop(guard);
}
fn append(text: &Text, priority: u8) {
    let mut store = STORE.lock();
    if text.truncated {
        TRUNCATED.fetch_add(1, Ordering::Relaxed);
    }
    store.append(text, priority);
    // Readers observe retained bytes even when console output is disabled or
    // its cursor has fallen behind. Wake only from a deferred safe point: log
    // producers may hold scheduler locks or run in interrupt context.
    READERS_PENDING.store(true, Ordering::Release);
    // Publish under the same lock as the consumer's empty check; no missed
    // wake.  A record no console's level would print is not work for that
    // console, which is what makes the level a cost decision and not only a
    // legibility one: `loglevel=1` costs a slow machine nothing at all.
    for id in CONSOLES {
        if store.prints(id, priority) {
            PENDING[id as usize].store(true, Ordering::Release);
        }
    }
}
/// Publish the record that reopens a rate-limit window, carrying the count of
/// what the closed window refused. Linux reports the same thing as its own line
/// (`net_ratelimit: N callbacks suppressed`), because a suppressed record that
/// leaves no trace is indistinguishable from a kernel that did not say it.
fn publish_suppressed(missed: u32, priority: u8) {
    let mut text = Text::with_reserved_leader();
    let _ = write!(
        text,
        "** {missed} kernel log message{} suppressed **",
        if missed == 1 { "" } else { "s" }
    );
    text.attach_leader(priority);
    text.finish();
    publish(&text, priority);
}
/// One numeric header field: the number, or `-` when the record has none yet.
///
/// Formatting an `Option` with `{:?}` spent eight bytes saying `Some(0)` on every
/// record whose number a reader could already read, and four saying `None` on the
/// pre-multitask boot records that make up most of a boot. `-` says the same in
/// one byte, and the byte it saves is ring capacity like any other.
fn write_field(text: &mut Text, value: Option<u64>) {
    match value {
        Some(value) => {
            let _ = write!(text, "{value}");
        }
        None => {
            let _ = text.write_str("-");
        }
    }
}
/// Render one `log` record: the bytes the ring keeps, and the priority the
/// consoles compare against.
///
/// The three context values are arguments rather than calls because the ring's
/// host tests must be able to check the bytes this produces, and a clock read
/// through the HAL is not something they can answer for.
fn render(
    record: &Record<'_>,
    time: core::time::Duration,
    cpu: Option<usize>,
    tid: Option<u64>,
) -> (Text, u8) {
    // The body first, into a buffer whose first three bytes are reserved: a
    // priority written as a `\x01N` prefix is part of the formatted text, so the
    // leader cannot be decided until the message exists.
    let mut text = Text::with_reserved_leader();
    let _ = write!(text, "[{}.{:06} cpu=", time.as_secs(), time.subsec_micros());
    write_field(&mut text, cpu.map(|cpu| cpu as u64));
    let _ = write!(text, " tid=");
    write_field(&mut text, tid);
    // `target=` alone: `log`'s macros default a record's target to its module
    // path, so the second field repeated the first on every record this kernel
    // writes, and a call site that *does* name a target has said that the
    // capture filter and the consoles consult is the only answer that matters.
    let _ = write!(
        text,
        " {} target={}] ",
        record.level(),
        record.target(),
    );
    let message = text.len;
    let _ = write!(text, "{}", record.args());
    let (explicit, skip) = parsed_priority(&text.bytes[message..text.len]);
    let mut priority = priority(record.level());
    if skip != 0 {
        text.remove(message, skip);
        if let Some(explicit) = explicit {
            priority = explicit;
        }
    }
    text.attach_leader(priority);
    text.finish();
    (text, priority)
}
struct Logger;
impl Log for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        emits(metadata.level(), metadata.target())
    }
    fn log(&self, record: &Record<'_>) {
        // Filter first: most records a busy kernel produces are in the band the
        // capture cap has closed, and there is no reason to format or to
        // serialize them.
        if !emits(record.level(), record.target()) {
            return;
        }
        use axlog::LogIf;
        let time = <crate::LogIfImpl as LogIf>::current_time();
        // Then the budget, before the buffer: a record this path refuses should
        // not have cost a kilobyte of formatting and a trip through the ring.
        //
        // Only the debug band meets a budget, and it is the `log` level's band
        // that decides, not the effective priority. A prefix can still take a
        // record to priority 0 from the debug band -- `debug!("\x010…")` is
        // stored at 0 and reaches every console -- but it cannot take it out of
        // the band's budget, which is the intended reading: what a prefix states
        // is how severe one message is, and a call site that can be reached once
        // per packet must not buy an unlimited run of the ring by claiming to be
        // the panic it is describing.
        //
        // `error`, `warn` and `info` are outside the budget by the same rule
        // that puts them outside the capture cap. Putting them inside it costs
        // two IRQ-off global locks to a record produced once per reclaimed page,
        // which is a slowdown of the allocator by the log, and the log does not
        // get to do that.
        let band = priority(record.level());
        let mut reopened = None;
        if band > INFO {
            match admits(
                record.file(),
                record.line(),
                time.as_millis().min(u64::MAX as u128) as u64,
            ) {
                Admit::No => return,
                Admit::YesWithNotice(missed) => reopened = Some(missed),
                Admit::Yes => {}
            }
        }
        let (text, priority) = render(
            record,
            time,
            <crate::LogIfImpl as LogIf>::current_cpu_id(),
            <crate::LogIfImpl as LogIf>::current_task_id(),
        );
        if let Some(missed) = reopened {
            publish_suppressed(missed, band);
        }
        publish(&text, priority);
    }
    fn flush(&self) {} // Never access the UART in an arbitrary caller's context.
}
pub(crate) fn init(level: &str) {
    set_console_supported(ConsoleId::Serial, axhal::console::diagnostic_available());
    // `level` can now come from the bootloader's `loglevel=`, so a typo is a
    // thing a human does rather than a build-time constant that was reviewed.
    // Say so, loudly enough to survive a quiet console: silently keeping the
    // default would present exactly as "the boot parameter had no effect", with
    // nothing to distinguish a rejected filter from one that was never read.
    if set_filter(level).is_err() {
        diagnostic_at(
            ERROR,
            format_args!(
                "klog: rejected log filter {level:?}; keeping {}",
                level_name(FILTER.lock().default)
            ),
        );
    }
    log::set_logger(&Logger).expect("kernel logger already installed");
    // `set_filter` moves the facade's gate with the filter it just installed;
    // this covers the branch above, where the filter stayed at its default and
    // nothing had yet told `log` what that default admits.
    let gate = FILTER.lock().facade_gate();
    log::set_max_level(gate);
    // After the filter, so a rejected one still leaves the console statements
    // applied: `quiet loglevel=oops` is a request to be quiet with a typo in it,
    // and the typo is not a reason to ignore the part that parsed.
    apply_console_command_line();
}
/// An explicit boot diagnostic at a chosen priority.
///
/// These bypass the capture filter, which is a decision about the `debug` band,
/// and the rate limits, which guard against a storm a call site can reach twice.
/// A boot says each of these once.
pub fn diagnostic_at(priority: u8, args: fmt::Arguments<'_>) {
    let mut text = Text::new();
    let _ = text.write_fmt(args);
    text.finish();
    publish(&text, priority);
}
/// A boot diagnostic at `KERN_NOTICE`: worth keeping, not worth alarming anyone.
pub fn diagnostic(args: fmt::Arguments<'_>) {
    diagnostic_at(NOTICE, args);
}
/// What the machine says as it stops, at `KERN_EMERG`.
///
/// A reboot, a halt and a power-off have one reader in mind: whoever is standing
/// at the box when it goes away.  `KERN_EMERG` is the only priority a quietened
/// console still shows -- `SYSLOG_ACTION_CONSOLE_OFF` installs
/// `minimum_console_loglevel` (1) and `quiet` installs 4 -- so a notice at any
/// other priority is one the command line can hide.
///
/// The record is both retained, which hands each console its ordinary copy, and
/// written to the diagnostic transport here: the caller stops the machine on the
/// next statement, and a line queued for a worker that never runs again is the
/// failure this primitive exists to remove.  A serial console whose worker does
/// get a turn prints the same line twice, which is the cheaper of the two
/// mistakes.  [`fatal`] is the same promise on a path that cannot even trust the
/// ring to be answerable.
pub fn death_notice(args: fmt::Arguments<'_>) {
    let mut text = Text::new();
    let _ = text.write_fmt(args);
    text.finish();
    publish(&text, EMERG);
    write_now(&text);
}
/// Send one record to the diagnostic transport from this CPU, now.
///
/// Bounded and never waiting: `try_write_diagnostic_bytes` takes what the port
/// will accept without spinning on its lock, and a machine that is stopping has
/// no patience to spend on an output that may already be gone.
fn write_now(text: &Text) {
    let bytes = text.as_bytes();
    let mut written = 0;
    while written != bytes.len() {
        let room = bytes.len() - written;
        let attempt = axhal::console::try_write_diagnostic_bytes(&bytes[written..]).min(room);
        written += attempt;
        if attempt == 0 {
            // The port is refusing bytes -- its lock is held, or the hardware is
            // gone -- and this bounded budget is the whole of the patience a
            // dying machine can spend on it.
            break;
        }
    }
}
/// The machine's own death report, said before it stops.
///
/// Call sites reach this when they have decided they cannot let the kernel keep
/// running, so nothing that follows them will ever schedule the console worker
/// again: the record is written to the diagnostic transport here rather than
/// handed to a reader that would print it later. It is also retained at
/// `KERN_EMERG` -- for the panic screen's tail row, and for a `dmesg` that some
/// other CPU still answers for -- but retention is taken only if the ring
/// answers at once. A CPU which stops the machine while waiting for a ring that
/// another CPU holds would leave nothing behind but a powered-off box, which is
/// the failure this notice exists to remove.
pub fn fatal(args: fmt::Arguments<'_>) -> ! {
    let mut text = Text::new();
    // One anchor for every site, so the line that explains a stopped machine is
    // recognisable by a reader who was not told which of them fired -- `tools/
    // qemu_runner/process.py` treats it as a crash, which is what the verification
    // suites need instead of a case timeout with a silent tail.
    let _ = text.write_str("TheKernel fatal: ");
    let _ = text.write_fmt(args);
    text.finish();
    match STORE.try_lock() {
        Some(mut store) => store.append(&text, EMERG),
        None => {
            LOST_RECORDS.fetch_add(1, Ordering::Relaxed);
        }
    }
    write_now(&text);
    axhal::power::system_off()
}
/// Legacy ax_print fragments are diagnostics, never terminal output.
pub fn record(bytes: &[u8]) {
    diagnostic(format_args!(
        "{}",
        core::str::from_utf8(bytes).unwrap_or("[invalid diagnostic UTF-8]")
    ));
}
pub fn snapshot_into(cursor: u64, dst: &mut [u8], newest: bool) -> (usize, u64) {
    STORE.lock().snapshot(cursor, dst, newest)
}
/// The same snapshot, refused rather than waited for when the ring is busy.
///
/// The panic handler runs on any CPU, at any point, possibly having interrupted
/// the very producer which holds the ring.  Spinning there would turn a legible
/// panic into a silent hang, so it takes the tail it can get and draws the rest
/// of the screen without it.
pub fn try_snapshot_into(cursor: u64, dst: &mut [u8], newest: bool) -> Option<(usize, u64)> {
    Some(STORE.try_lock()?.snapshot(cursor, dst, newest))
}
pub fn available_from(cursor: u64) -> usize {
    let store = STORE.lock();
    (store.end - cursor.max(store.oldest).min(store.end)) as usize
}
/// Report whether a console exists at all.
///
/// Two different questions reach this: the platform's probe (`init` says whether
/// the machine has a serial port) and a reader announcing itself (the framebuffer
/// console says so when it installs).  Either way a console that is not there
/// cannot show a record, so it is not owed one either -- which is what keeps
/// `log_stats`' per-console drop counts about a console a human can look at.
pub fn set_console_supported(id: ConsoleId, supported: bool) {
    STORE.lock().console_mut(id).supported = supported;
}
/// `console_loglevel`: the level every console compares a record's priority
/// against, and the one number `SYSLOG_ACTION_CONSOLE_*` moves in Linux.
///
/// Stored verbatim, as `console_loglevel` is there, where `proc_dointvec` does
/// not clamp either. `1..=8` is the meaningful band -- 1 shows only
/// `KERN_EMERG`, 8 shows the debug band -- and 0 is not rejected: it is a
/// console that prints nothing, which `/proc/sys/kernel/printk` and
/// `loglevel=0` can ask for (`SYSLOG_ACTION_CONSOLE_OFF` installs 1). That
/// costs nothing a panic needs, because the panic screen reads the retained ring
/// (`try_snapshot_into`) rather than a console.
pub fn set_console_loglevel(level: u8) {
    STORE.lock().console_loglevel = level;
}
pub fn console_loglevel() -> u8 {
    STORE.lock().console_loglevel
}
/// What one command-line token says about the console level, if anything.
///
/// The pure half of [`apply_console_command_line`], so the parse is testable
/// without a bootloader.
fn console_statement(token: &str) -> Option<u8> {
    match token {
        "quiet" => return Some(CONSOLE_LOGLEVEL_QUIET),
        "debug" => return Some(CONSOLE_LOGLEVEL_DEBUG),
        _ => {}
    }
    let (key, value) = token.split_once('=')?;
    if key != "loglevel" {
        return None;
    }
    // Out of `u8`'s range is not a level, and Linux's `loglevel=` rejects it the
    // same way rather than clamping.
    value.parse::<u8>().ok()
}
/// Apply the command line's console statements, in the order they appear.
///
/// `quiet`, `debug` and `loglevel=N` all write one number in Linux --
/// `console_loglevel`, which every console compares against -- so the one that
/// takes effect is the last of them on the line. Applying them in a fixed order
/// instead would make `quiet loglevel=8` mean the opposite of what it says.
///
/// A `loglevel=` whose value is not an integer is the capture-filter grammar this
/// kernel hangs on the same name ([`set_filter`]), and sets no console level: the
/// two are told apart by type, because no filter's default level is a number.
fn apply_console_command_line() {
    let Some(line) = axhal::boot::command_line() else {
        return;
    };
    for token in line.split_ascii_whitespace() {
        if let Some(level) = console_statement(token) {
            set_console_loglevel(level);
        }
    }
}
/// Retire a console whose reader has died, without affecting log retention.
pub fn retire_console(id: ConsoleId) {
    STORE.lock().retire(id);
    PENDING[id as usize].store(false, Ordering::Release);
}
/// Consume the coalesced log-arrival edge from a scheduler-safe dispatcher.
pub fn take_reader_notification() -> bool {
    READERS_PENDING.swap(false, Ordering::AcqRel)
}
/// Has a record this console would print arrived since its reader last looked?
pub fn console_work_pending(id: ConsoleId) -> bool {
    PENDING[id as usize].load(Ordering::Acquire)
}

/// One console's place in the retained ring.
///
/// A single task owns this cursor.  It prints what the ring retains, in order,
/// and what it has not printed yet stays in the ring: there is no second,
/// smaller buffer in which a record the console could not keep up with would be
/// lost.  A short UART write preserves its suffix, so a record that only partly
/// left the port is resumed rather than reprinted.
///
/// The level filter lives here rather than in the caller because it is decided
/// against the record's retained priority, which only a reader that delimits
/// records can see: a reader that copies the ring as a byte stream, as
/// [`snapshot_into`] does, cannot skip a record it has been told not to show.
pub struct ConsoleDrain {
    id: ConsoleId,
    /// Ring offset the console has secured: every record before it has been
    /// printed or is held in `pending`, whose bytes are already copied out.
    cursor: u64,
    pending: Option<Queued>,
    offset: usize,
}
impl ConsoleDrain {
    /// A reader that has seen nothing yet, which replays everything the ring
    /// still holds.
    pub const fn new(id: ConsoleId) -> Self {
        Self {
            id,
            cursor: 0,
            pending: None,
            offset: 0,
        }
    }
    pub fn id(&self) -> ConsoleId {
        self.id
    }
    /// How far this reader has secured the ring, for a caller that waits on the
    /// ring growing rather than on a wake edge.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
    /// The serial console's turn: one record, one bounded UART call.
    pub fn drain_serial_once(&mut self) -> usize {
        self.drain_with(axhal::console::try_write_diagnostic_bytes)
    }
    pub fn drain_with(&mut self, mut sink: impl FnMut(&[u8]) -> usize) -> usize {
        // At most one record and one finite sink call per turn.
        let Some(mut store) = STORE.try_lock() else {
            return 0;
        };
        let index = self.id as usize;
        if !store.console(self.id).shows() {
            // No console here, or its reader is gone.  Hold the cursor at the
            // end rather than build a backlog a later console would replay: a
            // level or on/off control governs what is printed, never what is
            // retained, and a console never reprints what it was quiet for.
            self.pending = None;
            self.offset = 0;
            self.cursor = store.end;
            store.console_mut(self.id).secured = store.end;
            PENDING[index].store(false, Ordering::Release);
            return 0;
        }
        while self.pending.is_none() {
            let Some((record, next)) = store.peek(self.cursor) else {
                break;
            };
            self.cursor = next;
            // The record is copied out here, so the console has secured it as
            // far as the ring is concerned even if the sink takes several
            // turns over it.
            store.console_mut(self.id).secured = next;
            if store.prints(self.id, record.priority) {
                self.pending = Some(record);
            }
        }
        if self.pending.is_none() {
            // Nothing printable is left: the console has secured the whole ring.
            // A cursor behind the oldest retained byte lands here when the only
            // text left is the tail of a record whose head the ring overwrote;
            // those records were counted lost when they were overwritten.
            self.cursor = store.end;
            store.console_mut(self.id).secured = store.end;
            PENDING[index].store(false, Ordering::Release);
        }
        drop(store);
        if let Some(record) = self.pending.as_ref() {
            let bytes = &record.text.as_bytes()[self.offset..];
            let written = sink(bytes).min(bytes.len());
            self.offset += written;
            if self.offset == record.text.len {
                self.pending = None;
                self.offset = 0;
            }
            return written;
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The ring the tests build for themselves.  The production one is
    /// [`CAPACITY`], which is far more than any test here needs and more than a
    /// host test thread's stack should be asked to hold twice -- two of these
    /// tests copy a `Store` or its `marks` beside the one they are checking.  The
    /// rules a ring has do not depend on how big it is, so the tests keep the
    /// size they were written and measured against.
    const LOCAL_CAPACITY: usize = 64 * 1024;
    type LocalStore = Store<LOCAL_CAPACITY>;
    /// The severity below `error`, named because the priority tables below read
    /// as a scale. Production has no comparison against it yet -- `diagnostic_at`
    /// takes the number from its caller -- so it lives here.
    const CRIT: u8 = 2;
    /// Most tests build their own `Store`; the ones below drive the real ring,
    /// filter and console, so they take this in case the harness is ever run
    /// with more than one test thread.
    static GLOBAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn global() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL.lock().unwrap_or_else(|error| error.into_inner())
    }
    #[test]
    fn ring_wrap_and_tail_cursor() {
        let mut store = LocalStore::new();
        let mut text = Text::new();
        text.write_str(&"x".repeat(RECORD_BYTES - 1)).unwrap();
        text.finish();
        for _ in 0..70 {
            store.append(&text, INFO);
        }
        assert_eq!(store.end, 70 * RECORD_BYTES as u64);
        assert_eq!(store.end - store.oldest, LOCAL_CAPACITY as u64);
        let mut dst = [0; 4];
        let (n, end) = store.snapshot(0, &mut dst, true);
        assert_eq!(n, 4);
        assert_eq!(end, store.end);
        assert_eq!(&dst, b"xxx\n");
    }
    #[test]
    fn concurrent_records_are_not_interleaved() {
        let store = std::sync::Arc::new(SpinNoIrq::new(LocalStore::new()));
        let mut threads = std::vec::Vec::new();
        for producer in 0..4 {
            let store = store.clone();
            threads.push(std::thread::spawn(move || {
                let mut text = Text::new();
                write!(
                    text,
                    "{producer}{producer}{producer}{producer}{producer}{producer}{producer}"
                )
                .unwrap();
                text.finish();
                for _ in 0..128 {
                    store.lock().append(&text, 6);
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let mut bytes = [0; 4096];
        let (n, end) = store.lock().snapshot(0, &mut bytes, false);
        assert_eq!((n, end), (4096, 4096));
        for record in bytes.chunks_exact(8) {
            assert!(record[..7].iter().all(|byte| *byte == record[0]));
            assert_eq!(record[7], b'\n');
        }
    }
    #[test]
    fn filters_are_bounded_and_longest_prefix_wins() {
        let filter = Filter::parse("warn,kernel=debug,kernel::tty=trace").unwrap();
        assert_eq!(filter.level("other"), LevelFilter::Warn);
        assert_eq!(filter.level("kernel::io"), LevelFilter::Debug);
        assert_eq!(filter.level("kernel::tty::read"), LevelFilter::Trace);
        // A term names path segments, not a word inside one: `kernel` must not
        // reach `kernelish`, or "every filter is a module path" stops being true.
        assert_eq!(filter.level("kernelish::foo"), LevelFilter::Warn);
        assert_eq!(filter.level("kernel"), LevelFilter::Debug);
        for invalid in [
            "",
            "info,",
            "info,=debug",
            "info,x=oops",
            "info,x=debug,x=trace",
        ] {
            assert!(Filter::parse(invalid).is_err());
        }
        assert!(Filter::parse(&format!("info,{}=trace", "x".repeat(65))).is_err());
        let mut excessive = std::string::String::from("info");
        for i in 0..17 {
            write!(excessive, ",t{i}=debug").unwrap();
        }
        assert!(Filter::parse(&excessive).is_err());
    }
    /// The gate the `log` facade is kept at never closes the band this kernel
    /// retains unconditionally, and it never opens the debug band further than
    /// the filter does.
    #[test]
    fn the_facade_gate_follows_the_filter_and_never_silences_the_log() {
        assert_eq!(
            Filter::parse("off").unwrap().facade_gate(),
            LevelFilter::Info
        );
        assert_eq!(
            Filter::parse("error").unwrap().facade_gate(),
            LevelFilter::Info
        );
        assert_eq!(
            Filter::parse("info,a=debug").unwrap().facade_gate(),
            LevelFilter::Debug
        );
        assert_eq!(
            Filter::parse("info,a=trace").unwrap().facade_gate(),
            LevelFilter::Trace
        );
        assert_eq!(
            Filter::parse("trace").unwrap().facade_gate(),
            LevelFilter::Trace
        );
        // `emits` and the gate agree at the boundary: what the facade lets
        // through at `info` is exactly what needs no filter consultation.
        set_filter("off").unwrap();
        assert!(emits(Level::Error, "anything"));
        assert!(emits(Level::Warn, "anything"));
        assert!(emits(Level::Info, "anything"));
        assert!(!emits(Level::Debug, "anything"));
        set_filter("info,a=debug").unwrap();
        assert!(emits(Level::Debug, "a::b"));
        assert!(!emits(Level::Debug, "c"));
        assert!(!emits(Level::Trace, "a"));
        set_filter("info").unwrap();
    }
    /// The record's leader is the last thing written, because a priority the
    /// author names with a `\x01N` prefix is part of the message and is not
    /// known until the message is.  Without a prefix the leader is the `log`
    /// level's own priority, which is the byte-for-byte contract every reader of
    /// the serial log has always had.
    #[test]
    fn a_records_priority_leader_replaces_the_prefix_it_was_asked_with() {
        let args = format_args!("\x013link down");
        let mut builder = Record::builder();
        builder
            .args(args)
            .level(Level::Info)
            .target("tk_kernel::net")
            .module_path(Some("tk_kernel::net"))
            .file(Some("net.rs"))
            .line(Some(7));
        let record = builder.build();
        let (text, priority) = render(
            &record,
            core::time::Duration::new(1, 200_000),
            Some(2),
            Some(9),
        );
        assert_eq!(priority, ERROR);
        assert_eq!(
            core::str::from_utf8(text.as_bytes()).unwrap(),
            // Byte-for-byte the record the ring keeps, with the leader now
            // carrying the effective priority rather than the `log` level's own,
            // and with the two header fields that said nothing: `module=`, which
            // `log` gives the same string as `target=`, and the `Some(..)`
            // wrapping of two numbers a reader could already read.
            "<3>[1.000200 cpu=2 tid=9 INFO target=tk_kernel::net] link down\n"
        );
        // A record from before the scheduler or a task exists says so with `-`,
        // which is what most of a boot's records are: the four bytes of `None`
        // were charged against the ring twice per record, on every one of them.
        let (earliest, _) = render(&record, core::time::Duration::new(1, 200_000), None, None);
        assert_eq!(
            core::str::from_utf8(earliest.as_bytes()).unwrap(),
            "<3>[1.000200 cpu=- tid=- INFO target=tk_kernel::net] link down\n"
        );

        let plain = format_args!("no prefix here");
        let mut builder = Record::builder();
        builder
            .args(plain)
            .level(Level::Warn)
            .target("t")
            .module_path(Some("m"));
        let record = builder.build();
        let (text, priority) = render(
            &record,
            core::time::Duration::new(1, 200_000),
            Some(2),
            Some(9),
        );
        assert_eq!(priority, WARNING);
        assert!(text.as_bytes().starts_with(b"<4>["));
    }
    /// A `\x01` that does not introduce a level is text, not severity: the
    /// parser stops at it and leaves it where the author put it, because a
    /// message cannot be trusted to re-decide its own priority by accident.
    #[test]
    fn a_priority_prefix_is_parsed_stripped_and_otherwise_left_alone() {
        for (body, expected) in [
            (&b"\x015beware"[..], (Some(5), 2)),
            (&b"\x010panic"[..], (Some(0), 2)),
            // The first header wins and every leading one is stripped: a
            // message assembled from an already-prefixed piece must not carry
            // the earlier one into the text.
            (&b"\x013\x017both"[..], (Some(3), 4)),
            (&b"plain"[..], (None, 0)),
            (&b"\x01"[..], (None, 0)),
            (&b"\x019nine"[..], (None, 0)),
            (&b"\x018"[..], (None, 0)),
            // The walk is bounded: a fifth header is text, not a level, and a
            // message that turns out to be a run of `\x01` bytes cannot make
            // this loop spin.
            (&b"\x013\x013\x013\x013\x013text"[..], (Some(3), 8)),
        ] {
            assert_eq!(parsed_priority(body), expected, "{body:?}");
        }
    }
    #[test]
    fn stripping_a_prefix_closes_the_gap_it_made() {
        let mut text = Text::new();
        text.write_str("ab\x015cd").unwrap();
        text.remove(2, 2);
        text.finish();
        assert_eq!(text.as_bytes(), b"abcd\n");
    }
    /// The scale the two axes share: a `log` level picks a priority, and
    /// `console_loglevel` decides which priorities a console shows.  `quiet` is
    /// the case a human actually sets, so it is the one checked by name.
    #[test]
    fn a_quiet_console_still_prints_the_machines_own_death_report() {
        let at = |loglevel: u8| {
            let mut store = LocalStore::new();
            store.console_loglevel = loglevel;
            store
        };
        let serial = ConsoleId::Serial;
        // `CONSOLE_LOGLEVEL_DEFAULT` = 7: everything up to `info`, and the debug
        // band only once someone has asked for it.
        let normal = at(CONSOLE_LOGLEVEL_DEFAULT);
        for priority in [CRIT, ERROR, WARNING, NOTICE, INFO] {
            assert!(
                normal.prints(serial, priority),
                "priority {priority} at default"
            );
        }
        assert!(
            !normal.prints(serial, DEBUG),
            "debug prints at the default level"
        );
        // `quiet`: `warn` and above are muted, and the machine's own death report
        // is not.  This is the whole reason the scale reaches below `error` -- a
        // level a human sets must not be able to hide a panic.
        let quiet = at(CONSOLE_LOGLEVEL_QUIET);
        for priority in [WARNING, NOTICE, INFO, DEBUG] {
            assert!(
                !quiet.prints(serial, priority),
                "priority {priority} survives quiet"
            );
        }
        for priority in 0..=ERROR {
            assert!(
                quiet.prints(serial, priority),
                "quiet mutes priority {priority}"
            );
        }
        // The floor `syslog(2)` allows leaves only `KERN_EMERG`, and 8 is what
        // opens the debug band.
        assert!(at(CONSOLE_LOGLEVEL_MIN).prints(serial, 0));
        assert!(!at(CONSOLE_LOGLEVEL_MIN).prints(serial, 1));
        for priority in 0..=DEBUG {
            assert!(at(8).prints(serial, priority), "priority {priority} at 8");
        }
        // A console the machine does not have shows nothing at any level, and so
        // does one whose reader has died: the level is not the only reason a
        // console is quiet, and `append` must not owe a debt to either.
        let absent = LocalStore::new();
        for priority in 0..=DEBUG {
            assert!(!absent.prints(ConsoleId::Screen, priority));
        }
        let mut retired = at(8);
        retired.retire(serial);
        for priority in 0..=DEBUG {
            assert!(!retired.prints(serial, priority));
        }
    }
    /// The rate limits are the price of unconditional retention: a call site
    /// that can be reached once per packet gets a window, not a free pass.
    #[test]
    fn a_chatty_callsite_gets_a_window_not_a_free_run_of_the_ring() {
        let _shared = global();
        reset_rate_state();
        set_ratelimit_interval_ms(5_000);
        set_ratelimit_burst(3);
        for expected in [Admit::Yes, Admit::Yes, Admit::Yes, Admit::No] {
            assert_eq!(admits(Some("f.rs"), Some(11), 1_000), expected);
        }
        // The record that reopens the window carries the count of the ones the
        // closed window refused: a suppression that leaves no trace is
        // indistinguishable from a kernel that never said it.
        assert_eq!(
            admits(Some("f.rs"), Some(11), 6_000),
            Admit::YesWithNotice(1)
        );
        // A different call site is not made to pay for this one's noise.
        assert_eq!(admits(Some("f.rs"), Some(12), 6_000), Admit::Yes);
        assert_eq!(admits(Some("g.rs"), Some(11), 6_000), Admit::Yes);
        set_ratelimit_interval_ms(DEFAULT_RATELIMIT_INTERVAL_MS);
        set_ratelimit_burst(DEFAULT_RATELIMIT_BURST);
    }
    /// Per-callsite windows cannot bound total volume, and with retention
    /// unconditional the ring is then the only thing between a storm and a boot
    /// that erased its own evidence.
    #[test]
    fn a_storm_of_distinct_callsites_cannot_outrun_the_ring() {
        let _shared = global();
        reset_rate_state();
        set_ratelimit_interval_ms(5_000);
        set_ratelimit_burst(10);
        // More distinct call sites than there are slots, each one far below its
        // own burst: only a guard over all of them together can stop this.
        let mut admitted = 0;
        for line in 0..(STORM_BURST + 44) {
            if admits(Some("f.rs"), Some(line), 1_000) == Admit::Yes {
                admitted += 1;
            }
        }
        assert_eq!(admitted, STORM_BURST, "the storm guard let volume through");
        assert_eq!(
            RATE_DROPPED.load(Ordering::Relaxed),
            44,
            "`log_stats` would report a different suppression than happened"
        );
        // The guard announces itself when it reopens, riding along with the
        // first record it lets through.
        assert_eq!(
            admits(Some("f.rs"), Some(0), 6_000),
            Admit::YesWithNotice(44)
        );
        set_ratelimit_interval_ms(DEFAULT_RATELIMIT_INTERVAL_MS);
        set_ratelimit_burst(DEFAULT_RATELIMIT_BURST);
    }
    /// The command line reaches the log through two grammars that share one
    /// parameter name, and through three statements that share one number.
    #[test]
    fn the_command_line_says_the_console_level_and_the_filter_separately() {
        assert_eq!(console_statement("loglevel=8"), Some(8));
        assert_eq!(console_statement("loglevel=0"), Some(0));
        assert_eq!(console_statement("loglevel=256"), None);
        assert_eq!(console_statement("loglevel="), None);
        assert_eq!(console_statement("loglevel"), None);
        // Not an integer, so this is the capture filter's grammar and sets no
        // console level.
        assert_eq!(console_statement("loglevel=warn"), None);
        assert_eq!(console_statement("quiet"), Some(CONSOLE_LOGLEVEL_QUIET));
        assert_eq!(console_statement("debug"), Some(CONSOLE_LOGLEVEL_DEBUG));
        // A flag spelled as a parameter is not that flag.
        assert_eq!(console_statement("quiet=1"), None);
        assert_eq!(console_statement("root=/dev/sda"), None);
        // `apply_console_command_line` applies them in line order, which is what
        // makes the last one the one that stands -- as it is in Linux, where all
        // three write the same `console_loglevel`.
        let last = |line: &str| {
            line.split_ascii_whitespace()
                .filter_map(console_statement)
                .last()
        };
        assert_eq!(last("quiet loglevel=8"), Some(8));
        assert_eq!(last("loglevel=8 quiet"), Some(CONSOLE_LOGLEVEL_QUIET));
        assert_eq!(last("debug"), Some(CONSOLE_LOGLEVEL_DEBUG));
        assert_eq!(last("root=/init loglevel=info"), None);
    }
    #[test]
    fn retention_is_independent_of_console_state() {
        // Console output is a reader, not a copy: muting it, or filtering what it
        // prints, never costs the retained log a byte.
        let mut store = LocalStore::new();
        let mut text = Text::new();
        text.write_str("message").unwrap();
        text.finish();
        store.console_mut(ConsoleId::Serial).supported = false;
        for _ in 0..80 {
            store.append(&text, ERROR);
        }
        assert_eq!(store.end, (80 * text.len) as u64);
        assert_eq!(store.console(ConsoleId::Serial).lost, 0);
        // A level the console would not print is retained all the same.
        store.console_mut(ConsoleId::Serial).supported = true;
        store.console_loglevel = WARNING;
        store.append(&text, INFO);
        assert_eq!(store.end, (81 * text.len) as u64);
        assert_eq!(store.console(ConsoleId::Serial).lost, 0);
        assert!(!store.prints(ConsoleId::Serial, INFO));
        assert!(store.prints(ConsoleId::Serial, ERROR));
    }
    /// A console the machine does not have, and one whose reader has died, are
    /// both quiet -- and quiet in the same way: they show nothing, they are owed
    /// nothing, and no later request can make the ring hand back what passed
    /// while they were gone.
    #[test]
    fn an_absent_console_and_a_retired_one_show_nothing_and_owe_nothing() {
        let mut store = LocalStore::new();
        let mut text = Text::new();
        text.write_str("retained").unwrap();
        text.finish();
        store.console_mut(ConsoleId::Serial).supported = false;
        assert!(!store.prints(ConsoleId::Serial, 0));
        store.append(&text, INFO);
        assert_eq!(store.end, text.len as u64);
        assert_eq!(store.console(ConsoleId::Serial).lost, 0);
        store.console_mut(ConsoleId::Serial).supported = true;
        assert!(store.prints(ConsoleId::Serial, INFO));
        store.append(&text, INFO);
        // Retiring pins the cursor where the ring is, so the wrap that follows
        // costs this console nothing it could still have been shown.
        store.retire(ConsoleId::Serial);
        assert!(!store.prints(ConsoleId::Serial, 0));
        assert_eq!(store.console(ConsoleId::Serial).secured, store.end);
        store.append(&text, INFO);
        assert_eq!(store.end, (3 * text.len) as u64);
        assert_eq!(store.console(ConsoleId::Serial).lost, 0);
    }
    #[test]
    fn bounded_text_terminates_and_marks_truncation() {
        let mut text = Text::new();
        text.write_str(&"é".repeat(RECORD_BYTES)).unwrap();
        text.finish();
        assert!(text.len <= RECORD_BYTES);
        assert!(core::str::from_utf8(text.as_bytes()).is_ok());
        assert!(text.as_bytes().ends_with(b" [truncated]\n"));
    }
    /// A console that keeps up loses nothing, however many times the ring
    /// wraps around it.  The count is about records the ring destroyed before
    /// the console could copy them out, not about how often the ring wrapped:
    /// a counter that grew with every wrap would make every long boot report
    /// dropped records that all reached the console.
    #[test]
    fn a_console_that_keeps_up_loses_nothing_however_often_the_ring_wraps() {
        let mut text = Text::new();
        text.write_str("twenty-four bytes long..\n").unwrap();
        text.finish();
        let per_record = text.len as u64;
        let records_to_wrap = (LOCAL_CAPACITY as u64 / per_record) as usize;

        let mut store = LocalStore::new();
        // Wrap the ring once with nobody reading, then let the console catch
        // up completely.  Everything the ring still holds is now secured.
        for _ in 0..records_to_wrap + 3 {
            store.append(&text, INFO);
        }
        store.console_mut(ConsoleId::Serial).secured = store.end;
        let after_catch_up = store.console(ConsoleId::Serial).lost;

        // Two more records force another wrap.  The heads they overwrite are
        // records the console has already copied out, so they are not losses.
        for _ in 0..2 {
            store.append(&text, INFO);
        }
        assert_eq!(
            store.console(ConsoleId::Serial).lost,
            after_catch_up,
            "the ring wrapped over records the console had already secured"
        );
    }

    /// The counter agrees with the definition it names: one more loss for each
    /// record head the ring destroys while the console had not secured it, and
    /// nothing else.
    #[test]
    fn the_lost_count_is_the_heads_the_ring_destroyed_unread() {
        let mut text = Text::new();
        text.write_str("twenty-four bytes long..\n").unwrap();
        text.finish();
        let per_record = text.len as u64;
        let records_to_wrap = (LOCAL_CAPACITY as u64 / per_record) as usize;

        for records in [records_to_wrap - 2, records_to_wrap, records_to_wrap + 9] {
            for secured in [0, per_record * 3 / 2, 40 * per_record, u64::MAX] {
                let mut store = LocalStore::new();
                for _ in 0..records {
                    store.append(&text, INFO);
                }
                let secured = secured.min(store.end);
                store.console_mut(ConsoleId::Serial).secured = secured;
                let before = store.console(ConsoleId::Serial).lost;
                let oldest = store.oldest;
                // The marks of the bytes this append is about to destroy.
                // Reading them afterwards would read the replacements.
                let doomed_marks = store.marks;
                store.append(&text, INFO);

                // One append moves `oldest` forward over the heads this record
                // destroyed.  A head sits at a ring offset whose mark says the
                // byte starts a record, and the console still owes it when the
                // head is at or past the console cursor.
                let mut destroyed_unread = 0;
                let mut at = oldest;
                while at < store.oldest {
                    let starts_record = doomed_marks[at as usize % LOCAL_CAPACITY] & RECORD_START != 0;
                    if starts_record && at >= secured {
                        destroyed_unread += 1;
                    }
                    at += 1;
                }
                assert_eq!(
                    store.console(ConsoleId::Serial).lost - before,
                    destroyed_unread,
                    "records={records} secured={secured} oldest={oldest}..{}",
                    store.oldest
                );
            }
        }
    }

    /// A run of records the level mutes never wakes the console, so its cursor
    /// can still be behind when the ring wraps over them.  Those records were
    /// never going to be shown, so the wrap costs the console nothing; one it
    /// would have printed, overwritten the same way, is still a loss.
    #[test]
    fn a_muted_record_the_ring_overwrites_is_not_a_console_loss() {
        let mut store = LocalStore::new();
        let mut text = Text::new();
        text.write_str("twenty-four bytes long..\n").unwrap();
        text.finish();
        let records_to_wrap = LOCAL_CAPACITY / text.len;
        store.append(&text, INFO);
        for _ in 0..records_to_wrap + 3 {
            store.append(&text, DEBUG);
        }
        assert!(!store.prints(ConsoleId::Serial, DEBUG));
        assert_eq!(
            store.console(ConsoleId::Serial).lost,
            1,
            "only the INFO record the console would have printed is lost"
        );
    }

    /// A record is only lost to the console when the ring overwrites it before
    /// the console reads it, and then it is counted, not silent.
    #[test]
    fn the_console_loses_a_record_only_when_the_ring_overwrites_it() {
        let mut store = LocalStore::new();
        let mut text = Text::new();
        text.write_str("twenty-four bytes long..\n").unwrap();
        text.finish();
        let per_record = text.len as u64;
        let records_to_wrap = (LOCAL_CAPACITY as u64 / per_record) as usize;
        for _ in 0..records_to_wrap + 3 {
            store.append(&text, INFO);
        }
        // Three records were overwritten while the console cursor never moved.
        assert_eq!(store.console(ConsoleId::Serial).lost, 3);
        assert_eq!(store.end, ((records_to_wrap + 3) as u64) * per_record);
        // What the ring still holds, the console can still read -- starting at
        // the first record whose head survived, not in the middle of one.
        assert_eq!(per_record, 25);
        let (record, next) = store.peek(0).unwrap();
        assert_eq!(record.text.as_bytes(), text.as_bytes());
        assert_eq!(next % per_record, 0, "a reader must resume at a boundary");
        // The oldest retained byte (64) falls inside the fourth record, so the
        // first readable record is that whole record, not its tail.
        assert!(store.oldest % per_record != 0);
        assert_eq!(next, 4 * per_record);
        assert!(next - per_record >= store.oldest);
    }
    /// The delimiter between records is the next record's start mark, not a
    /// newline.  A record's own text may contain newlines -- the igc driver's
    /// absence report is two lines in one `info!` -- and a reader that stopped
    /// at the first one returned the first line, left the cursor past the whole
    /// record and dropped the rest, which is how that report lost its verdict.
    #[test]
    fn a_record_whose_text_contains_newlines_is_read_whole() {
        let mut store = LocalStore::new();
        let mut text = Text::new();
        text.write_str("bus walk: nothing matched\nverdict: no supported device present\n")
            .unwrap();
        text.finish();
        store.append(&text, INFO);
        let (record, next) = store.peek(0).unwrap();
        assert_eq!(record.text.as_bytes(), text.as_bytes());
        assert_eq!(next, text.len as u64);
        assert_eq!(record.priority, INFO);
    }
    /// A reader that reaches a multi-line record must leave the cursor on the
    /// next record's first byte: stopping inside it would hand the following
    /// record to the reader as a tail, and that record is retained.
    #[test]
    fn a_multiline_record_does_not_consume_the_record_after_it() {
        let mut store = LocalStore::new();
        let mut first = Text::new();
        first.write_str("first line\nsecond line\n").unwrap();
        first.finish();
        let mut second = Text::new();
        second.write_str("following").unwrap();
        second.finish();
        store.append(&first, INFO);
        store.append(&second, INFO);
        let (record, next) = store.peek(0).unwrap();
        assert_eq!(record.text.as_bytes(), first.as_bytes());
        let (record, next) = store.peek(next).unwrap();
        assert_eq!(record.text.as_bytes(), second.as_bytes());
        assert_eq!(next, (first.len + second.len) as u64);
    }
    /// The newest record has no following start mark, so the reader's other
    /// boundary -- the newest retained byte -- is what stops it, and the
    /// `RECORD_BYTES` bound must not cut a record `Text` was allowed to build
    /// in full.
    #[test]
    fn a_record_ending_at_the_rings_end_is_read_whole() {
        let mut store = LocalStore::new();
        let mut text = Text::new();
        // `RECORD_BYTES` exactly once `finish` terminates it: the longest record
        // a producer can retain, and one that contains newlines of its own.
        text.write_str(&"l\n".repeat((RECORD_BYTES - 2) / 2))
            .unwrap();
        text.write_str("x").unwrap();
        text.finish();
        assert_eq!(text.len, RECORD_BYTES);
        for _ in 0..LOCAL_CAPACITY / RECORD_BYTES {
            store.append(&text, INFO);
        }
        assert_eq!(store.end, LOCAL_CAPACITY as u64);
        // The record whose last byte is the newest retained byte.
        let (record, next) = store.peek(store.end - RECORD_BYTES as u64).unwrap();
        assert_eq!(record.text.as_bytes(), text.as_bytes());
        assert_eq!(next, store.end);
        assert!(
            store.peek(next).is_none(),
            "nothing is retained past the newest byte"
        );
        // And the record the ring wrapped onto, at the ring's first byte, is a
        // record rather than the tail of one.
        let (record, next) = store.peek(0).unwrap();
        assert_eq!(record.text.as_bytes(), text.as_bytes());
        assert_eq!(next, RECORD_BYTES as u64);
    }
    /// A reader that has fallen behind resumes at the next record boundary, and
    /// a boundary is a start mark: the newline inside a multi-line record must
    /// not be mistaken for one, or the reader would print a record's tail as a
    /// record of its own.
    #[test]
    fn a_multiline_record_whose_head_the_ring_overwrote_is_skipped() {
        let mut store = LocalStore::new();
        let mut text = Text::new();
        text.write_str("record 000\nsecond line 000\n").unwrap();
        text.finish();
        let per_record = text.len as u64;
        assert_ne!(
            LOCAL_CAPACITY as u64 % per_record,
            0,
            "the test needs the ring to stop mid-record"
        );
        for _ in 0..LOCAL_CAPACITY as u64 / per_record + 3 {
            store.append(&text, INFO);
        }
        assert!(
            store.oldest % per_record != 0,
            "the test needs a cut record"
        );
        let (record, next) = store.peek(0).unwrap();
        assert_eq!(record.text.as_bytes(), text.as_bytes());
        assert_eq!(next % per_record, 0, "a reader must resume at a boundary");
        assert!(next - per_record >= store.oldest);
    }
    /// The console prints the record the ring retains, once, and stops where
    /// the UART stopped: a partly written multi-line record resumes at the byte
    /// after the one sent rather than reprinting a line or skipping the next
    /// record, and the byte-oriented reader the screen uses sees the same bytes.
    #[test]
    fn the_console_prints_a_multiline_record_once_and_resumes_where_it_stopped() {
        let _serial = global();
        // Other tests log too, so the shared arrival edge may already be set.
        let _ = take_reader_notification();
        set_console_loglevel(8);
        let mut drain = ConsoleDrain::new(ConsoleId::Serial);
        let start = STORE.lock().end;
        drain.cursor = start;
        diagnostic(format_args!("first line\nsecond line"));
        diagnostic(format_args!("third"));
        let mut printed = std::vec::Vec::new();
        // A UART that accepts three bytes per turn stops inside the first line.
        drain.drain_with(|bytes| {
            let n = 3.min(bytes.len());
            printed.extend_from_slice(&bytes[..n]);
            n
        });
        assert_eq!(printed, b"fir");
        drain.drain_with(|bytes| {
            printed.extend_from_slice(bytes);
            bytes.len()
        });
        assert_eq!(printed, b"first line\nsecond line\n");
        drain.drain_with(|bytes| {
            printed.extend_from_slice(bytes);
            bytes.len()
        });
        assert_eq!(printed, b"first line\nsecond line\nthird\n");
        // The readers that copy bytes rather than delimit records -- the early
        // screen and the framebuffer mirror -- agree with the console, byte for
        // byte: the record is printed once, not once per line and not twice.
        let mut mirror = [0u8; 64];
        let (n, end) = snapshot_into(start, &mut mirror, false);
        assert_eq!(&mirror[..n], &printed[..]);
        assert_eq!(end, start + printed.len() as u64);
    }
    /// The reported bug: a burst larger than a bounded console queue reached
    /// the ring and only part of it reached the console, so the serial log
    /// disagreed with `dmesg` about what the kernel had said.
    #[test]
    fn a_slow_console_prints_every_record_the_ring_retains() {
        let _serial = global();
        const BURST: usize = 200;
        // Other tests log too, so the shared arrival edge may already be set.
        let _ = take_reader_notification();
        set_filter("info").unwrap();
        set_console_loglevel(8);
        let before = STORE.lock().end;
        let lost_before = STORE.lock().console(ConsoleId::Serial).lost;
        for index in 0..BURST {
            diagnostic(format_args!("burst {index:04}"));
        }
        let mut drain = ConsoleDrain::new(ConsoleId::Serial);
        drain.cursor = before;
        let mut printed = std::vec::Vec::new();
        // A UART that accepts one byte per turn: far slower than the producer,
        // which is what used to fill the old 64-record console queue.
        let mut turns = 0;
        while console_work_pending(ConsoleId::Serial) && turns < 64 * BURST {
            drain.drain_with(|bytes| {
                printed.extend_from_slice(&bytes[..1]);
                1
            });
            turns += 1;
        }
        assert!(
            !console_work_pending(ConsoleId::Serial),
            "the console never caught up"
        );
        for index in 0..BURST {
            let line = format!("burst {index:04}\n");
            assert!(
                printed
                    .windows(line.len())
                    .any(|window| window == line.as_bytes()),
                "record {index} reached the ring but not the console"
            );
        }
        assert_eq!(
            STORE.lock().console(ConsoleId::Serial).lost,
            lost_before,
            "a console that is slow, not a ring that wrapped, must cost nothing"
        );
    }
    /// A nested producer -- an interrupt handler interrupting a log, say --
    /// hands its record to the producer that owns the CPU rather than losing
    /// it, and the owner publishes it on the way out.
    #[test]
    fn a_nested_producer_hands_its_record_to_the_outer_one() {
        let _serial = global();
        // Other tests log too, so the shared arrival edge may already be set.
        let _ = take_reader_notification();
        let end_before = STORE.lock().end;
        {
            let cpu = current_cpu();
            let preempt = kernel_guard::NoPreempt::new();
            assert!(!PRODUCING[cpu].swap(true, Ordering::Acquire));
            let owner = ProducerGuard {
                cpu,
                _preempt: preempt,
            };
            diagnostic(format_args!("nested record"));
            assert!(
                STORE.lock().end == end_before,
                "a nested producer must not publish while the owner holds the CPU"
            );
            drop(owner);
        }
        let mut drain = ConsoleDrain::new(ConsoleId::Serial);
        drain.cursor = end_before;
        let mut printed = std::vec::Vec::new();
        drain.drain_with(|bytes| {
            printed.extend_from_slice(bytes);
            bytes.len()
        });
        assert_eq!(printed, b"nested record\n");
    }
    /// A ring lock held by another CPU delays a producer; it does not cost the
    /// record.
    #[test]
    fn a_record_survives_a_held_ring_lock() {
        let _serial = global();
        // Other tests log too, so the shared arrival edge may already be set.
        let _ = take_reader_notification();
        let end_before = STORE.lock().end;
        let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let holder = {
            let held = held.clone();
            let release = release.clone();
            std::thread::spawn(move || {
                let _store = STORE.lock();
                held.store(true, Ordering::Release);
                // Bounded so a producer which waits (the fixed behaviour) and a
                // producer which does not cannot deadlock the test.
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
                while !release.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                    core::hint::spin_loop();
                }
            })
        };
        while !held.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        // The producer reaches this while the holder still owns the ring.  The
        // record must wait for it, not be refused: "twenty-four bytes could not
        // be copied at this instant" is not a reason to lose a log record.
        diagnostic(format_args!("contended record"));
        release.store(true, Ordering::Release);
        holder.join().unwrap();
        assert_eq!(
            STORE.lock().end,
            end_before + "contended record\n".len() as u64
        );
    }
    /// A filter lock held by another CPU delays the level decision; it does not
    /// turn a record that would have been kept into one dropped for no reason.
    #[test]
    fn a_busy_filter_delays_the_level_decision() {
        let _serial = global();
        set_filter("info").unwrap();
        let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let holder = {
            let held = held.clone();
            let release = release.clone();
            std::thread::spawn(move || {
                let _filter = FILTER.lock();
                held.store(true, Ordering::Release);
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
                while !release.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                    core::hint::spin_loop();
                }
            })
        };
        while !held.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        assert!(
            emits(Level::Info, "kernel"),
            "a record the filter keeps must not be refused because the filter was busy"
        );
        release.store(true, Ordering::Release);
        holder.join().unwrap();
    }
    #[test]
    fn reader_notification_and_partial_drain_resume() {
        let _serial = global();
        // Other tests log too, so the shared arrival edge may already be set.
        let _ = take_reader_notification();
        set_console_loglevel(8);
        let mut drain = ConsoleDrain::new(ConsoleId::Serial);
        drain.cursor = STORE.lock().end;
        diagnostic(format_args!("retained"));
        assert!(take_reader_notification());
        assert!(console_work_pending(ConsoleId::Serial));
        let mut output = std::vec::Vec::new();
        drain.drain_with(|bytes| {
            output.extend_from_slice(&bytes[..2.min(bytes.len())]);
            2.min(bytes.len())
        });
        assert_eq!(output, b"re");
        drain.drain_with(|bytes| {
            output.extend_from_slice(bytes);
            bytes.len()
        });
        assert_eq!(output, b"retained\n");
        // The flag is cleared by the turn that finds nothing left to print, so
        // the quiet turn is also what proves the console caught up.
        drain.drain_with(|_| panic!("an empty ring must not touch the UART"));
        assert!(!console_work_pending(ConsoleId::Serial));
        set_filter("warn,test=debug").unwrap();
        assert!(set_filter("info,test=oops").is_err());
        assert_eq!(FILTER.lock().default, LevelFilter::Warn);
        assert_eq!(FILTER.lock().level("test::sub"), LevelFilter::Debug);
        set_filter("info").unwrap();
    }
    /// The console level control governs what is printed, never what is kept,
    /// and switching the console off does not replay what it was quiet for.
    #[test]
    fn an_absent_console_prints_nothing_and_does_not_replay() {
        let _serial = global();
        // Other tests log too, so the shared arrival edge may already be set.
        let _ = take_reader_notification();
        let mut drain = ConsoleDrain::new(ConsoleId::Serial);
        set_console_loglevel(8);
        drain.cursor = STORE.lock().end;
        set_console_supported(ConsoleId::Serial, false);
        let end_before = STORE.lock().end;
        diagnostic(format_args!("quiet"));
        // Retention does not consult the console: the record is in the ring even
        // though nothing could have shown it.
        assert_eq!(STORE.lock().end, end_before + "quiet\n".len() as u64);
        // Other tests may have left printable work behind, so this asserts on the
        // sink being untouched rather than on the shared pending flag.
        drain.drain_with(|_| panic!("a console the machine lacks must not be read"));
        set_console_supported(ConsoleId::Serial, true);
        diagnostic(format_args!("audible"));
        let mut printed = std::vec::Vec::new();
        drain.drain_with(|bytes| {
            printed.extend_from_slice(bytes);
            bytes.len()
        });
        // What passed while there was no console is not owed to the one that
        // arrived after it: a console level and a console's existence govern what
        // is printed, never what is kept, and neither reprints what it was quiet
        // for.
        assert_eq!(printed, b"audible\n");
        set_console_loglevel(CONSOLE_LOGLEVEL_DEFAULT);
    }
    /// A console's level is a decision about printing, and the reader keeps its
    /// place in the ring either way: a record it was told not to show is one it
    /// has seen, not one it is still owed.  Otherwise a quiet console would hold
    /// the ring's tail hostage and report a loss for every record the ring
    /// wrapped over while it was muted.
    #[test]
    fn a_muted_record_is_read_past_rather_than_queued_up() {
        let _serial = global();
        let _ = take_reader_notification();
        let mut drain = ConsoleDrain::new(ConsoleId::Serial);
        drain.cursor = STORE.lock().end;
        set_console_loglevel(WARNING);
        // `diagnostic()` is `KERN_NOTICE`, above what this console will show.
        diagnostic(format_args!("unaudible"));
        let mut printed = std::vec::Vec::new();
        drain.drain_with(|bytes| {
            printed.extend_from_slice(bytes);
            bytes.len()
        });
        assert!(printed.is_empty(), "a muted record reached the console");
        assert_eq!(drain.cursor(), STORE.lock().end);
        assert!(
            !console_work_pending(ConsoleId::Serial),
            "a record the console will not print is not work for it"
        );
        // Turning the level back up does not replay what it was muted for.
        set_console_loglevel(8);
        diagnostic(format_args!("audible"));
        drain.drain_with(|bytes| {
            printed.extend_from_slice(bytes);
            bytes.len()
        });
        assert_eq!(printed, b"audible\n");
        set_console_loglevel(CONSOLE_LOGLEVEL_DEFAULT);
    }
}
