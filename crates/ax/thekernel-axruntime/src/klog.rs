//! Bounded kernel diagnostics. Producers never access a UART and never refuse a
//! record because another CPU happened to hold the ring.
//!
//! There are two outputs and they are fed from one place.  The **retained ring**
//! is the log every reader sees: `/dev/kmsg`, `syslog(2)`, the framebuffer
//! console mirror and the boot screen all read it through their own cursor.
//! The **diagnostic console** (the serial port, where a machine has one) used to
//! keep a second, bounded copy of each record in a 64-record queue, and a record
//! the queue had no room for was dropped from the console for good even though
//! the ring still held it: a boot burst could therefore reach `/dev/kmsg` and a
//! screen log but not the serial log, with only a counter to say so.  The
//! console is now a reader of the retained ring like the others -- it carries a
//! cursor, not a copy -- so the ring is the only bound, and losing text means
//! the ring itself overwrote it, which `log_stats` reports.
use core::{
    fmt::{self, Write},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use kspin::SpinNoIrq;
use log::{Level, LevelFilter, Log, Metadata, Record};

pub const CAPACITY: usize = 64 * 1024;
const RECORD_BYTES: usize = 1024;
const FILTERS: usize = 16;
const PREFIX_BYTES: usize = 64;
/// Set on the first byte of a record in [`Store::marks`].
const RECORD_START: u8 = 0x80;
/// The record's console priority, in the low bits of a [`Store::marks`] byte.
const PRIORITY_MASK: u8 = 0x07;

#[derive(Clone, Copy)]
struct Text {
    bytes: [u8; RECORD_BYTES],
    len: usize,
    truncated: bool,
}
impl Text {
    const fn new() -> Self {
        Self {
            bytes: [0; RECORD_BYTES],
            len: 0,
            truncated: false,
        }
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
        if self.len == 0 || self.bytes[self.len - 1] != b'\n' {
            self.bytes[self.len] = b'\n';
            self.len += 1;
        }
    }
    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
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
struct Store {
    bytes: [u8; CAPACITY],
    /// The priority of the record each retained byte belongs to, with
    /// [`RECORD_START`] set on a record's first byte.  A record's text does not
    /// carry its priority -- a `diagnostic()` fragment is arbitrary bytes -- and
    /// the console's level filter needs it when the console reaches the record,
    /// so it is kept beside the text instead of in a second copy of the record.
    marks: [u8; CAPACITY],
    oldest: u64,
    end: u64,
    /// How far the console has secured the ring: every record starting before
    /// this offset has been printed or is held by the reader that is printing
    /// it.  It is per console, not per reader, because only the console can
    /// fall behind the ring by itself; the other readers have their own cursors.
    console: u64,
    /// Records the ring overwrote before the console reached them: the only way
    /// a retained record can now fail to reach the console.
    console_lost: u64,
    enabled: bool,
    supported: bool,
    retired: bool,
    threshold: u8,
}
impl Store {
    const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            marks: [0; CAPACITY],
            oldest: 0,
            end: 0,
            console: 0,
            console_lost: 0,
            enabled: true,
            supported: true,
            retired: false,
            threshold: 8,
        }
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled && self.supported && !self.retired;
    }
    fn retire(&mut self) {
        self.retired = true;
        self.enabled = false;
        // A retired console prints nothing more, so nothing more can be lost
        // to it: hold its cursor at the end instead of counting every later
        // overwrite as a record it never printed.
        self.console = self.end;
    }
    /// Retain one record.
    ///
    /// The ring is the log and the only copy of it, so this never refuses a
    /// record: the console reads it later through its own cursor, and a console
    /// that cannot keep up costs nothing until the ring itself overwrites what
    /// the console had not reached.
    fn append(&mut self, text: &Text, priority: u8) {
        let priority = priority & PRIORITY_MASK;
        for (index, byte) in text.as_bytes().iter().enumerate() {
            let slot = self.end as usize % CAPACITY;
            if self.marks[slot] & RECORD_START != 0 && self.console <= self.end {
                // About to overwrite the first byte of a record the console had
                // not secured.  Count the record, not the event: `log_stats`
                // reports records, and a reader must be able to tell "the ring
                // wrapped" from "the console lost this many records".
                self.console_lost += 1;
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
        self.oldest = self.end.saturating_sub(CAPACITY as u64);
    }
    fn snapshot(&self, cursor: u64, dst: &mut [u8], newest: bool) -> (usize, u64) {
        let mut start = cursor.max(self.oldest).min(self.end);
        let n = dst.len().min((self.end - start) as usize);
        if newest {
            start = self.end - n as u64;
        }
        for (i, byte) in dst[..n].iter_mut().enumerate() {
            *byte = self.bytes[(start as usize + i) % CAPACITY];
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
        while at < self.end && self.marks[at as usize % CAPACITY] & RECORD_START == 0 {
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
        let priority = self.marks[at as usize % CAPACITY] & PRIORITY_MASK;
        let mut text = Text::new();
        while text.len < RECORD_BYTES {
            if at >= self.end {
                break;
            }
            // Once the first byte is copied, a start mark on the byte at `at`
            // belongs to the next record and is this record's end.
            if text.len > 0 && self.marks[at as usize % CAPACITY] & RECORD_START != 0 {
                break;
            }
            text.bytes[text.len] = self.bytes[at as usize % CAPACITY];
            text.len += 1;
            at += 1;
        }
        Some((Queued { text, priority }, at))
    }
}
static STORE: SpinNoIrq<Store> = SpinNoIrq::new(Store::new());
static PENDING: AtomicBool = AtomicBool::new(false);
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
            if entry.len > longest && target.as_bytes().starts_with(&entry.prefix[..entry.len]) {
                longest = entry.len;
                selected = entry.level;
            }
        }
        selected
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
                core::str::from_utf8(&entry.prefix[..entry.len]).unwrap(),
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

/// Replacement grammar: `info,target_prefix=debug,longer_prefix=trace`.
/// At most 16 unique ASCII prefixes, each at most 64 bytes. Longest prefix wins.
/// Invalid input leaves the previous configuration untouched; callers authorize writes.
pub fn set_filter(text: &str) -> Result<(), ()> {
    let next = Filter::parse(text)?;
    *FILTER.lock() = next;
    Ok(())
}
pub fn write_filter(out: &mut impl Write) -> fmt::Result {
    let snapshot = *FILTER.lock();
    snapshot.render(out)
}
pub fn write_stats(out: &mut impl Write) -> fmt::Result {
    let (overwritten, supported, retired, console_lost) = {
        let store = STORE.lock();
        (
            store.oldest,
            store.supported,
            store.retired,
            store.console_lost,
        )
    };
    writeln!(
        out,
        "records_dropped {}\ndiagnostic_records_dropped {}\nrecords_truncated \
         {}\nretention_bytes_overwritten {}\ndiagnostic_supported {}\ndiagnostic_retired {}",
        LOST_RECORDS.load(Ordering::Relaxed),
        console_lost,
        TRUNCATED.load(Ordering::Relaxed),
        overwritten,
        supported as u8,
        retired as u8
    )
}
fn allowed(level: Level, target: &str) -> bool {
    // Read the filter rather than refusing to read it.  The critical section is
    // a bounded scan of at most `FILTERS` prefixes, and a record thrown away
    // because another CPU was replacing the filter at that instant is a record
    // lost for no reason.
    level <= FILTER.lock().level(target)
}
fn priority(level: Level) -> u8 {
    match level {
        Level::Error => 3,
        Level::Warn => 4,
        Level::Info => 6,
        Level::Debug | Level::Trace => 7,
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
fn publish(text: &Text, level: Level) {
    let preempt = kernel_guard::NoPreempt::new();
    let cpu = current_cpu();
    if PRODUCING[cpu].swap(true, Ordering::Acquire) {
        let mut deferred = DEFERRED.lock();
        if deferred[cpu].is_none() {
            deferred[cpu] = Some(Queued {
                text: *text,
                priority: priority(level),
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
    append(text, priority(level));
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
    // wake.  A record the console's level filter would not print is not work.
    if store.enabled && priority < store.threshold {
        PENDING.store(true, Ordering::Release);
    }
}
struct Logger;
impl Log for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        allowed(metadata.level(), metadata.target())
    }
    fn log(&self, record: &Record<'_>) {
        // Filter first: most records a busy kernel produces are below the
        // capture level, and there is no reason to format or to serialize them.
        if !allowed(record.level(), record.target()) {
            return;
        }
        let mut text = Text::new();
        use axlog::LogIf;
        let time = <crate::LogIfImpl as LogIf>::current_time();
        let cpu = <crate::LogIfImpl as LogIf>::current_cpu_id();
        let tid = <crate::LogIfImpl as LogIf>::current_task_id();
        let _ = write!(
            text,
            "<{}>[{}.{:06} cpu={:?} tid={:?} {} target={} module={}] {}",
            priority(record.level()),
            time.as_secs(),
            time.subsec_micros(),
            cpu,
            tid,
            record.level(),
            record.target(),
            record.module_path().unwrap_or("?"),
            record.args()
        );
        text.finish();
        publish(&text, record.level());
    }
    fn flush(&self) {} // Never access the UART in an arbitrary caller's context.
}
pub(crate) fn init(level: &str) {
    {
        let mut store = STORE.lock();
        store.supported = axhal::console::diagnostic_available();
        store.set_enabled(true);
    }
    let _ = set_filter(level);
    log::set_logger(&Logger).expect("kernel logger already installed");
    log::set_max_level(LevelFilter::Trace);
}
/// Explicit boot diagnostics bypass the capture filter, but use the bounded sink.
pub fn diagnostic(args: fmt::Arguments<'_>) {
    let mut text = Text::new();
    let _ = text.write_fmt(args);
    text.finish();
    publish(&text, Level::Info);
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
pub fn set_console_enabled(enabled: bool) {
    STORE.lock().set_enabled(enabled);
}
/// Retire a failed diagnostic consumer without affecting log retention.
/// Further console-enable requests cannot enqueue work without a consumer.
pub fn retire_diagnostic_sink() {
    let mut store = STORE.lock();
    store.retire();
    PENDING.store(false, Ordering::Release);
}
pub fn set_console_threshold(threshold: u8) {
    STORE.lock().threshold = threshold;
}
/// Consume the coalesced log-arrival edge from a scheduler-safe dispatcher.
pub fn take_reader_notification() -> bool {
    READERS_PENDING.swap(false, Ordering::AcqRel)
}
pub fn diagnostic_work_pending() -> bool {
    PENDING.load(Ordering::Acquire)
}

/// The console's place in the retained ring.
///
/// A single task owns this cursor.  It prints what the ring retains, in order,
/// and what it has not printed yet stays in the ring: there is no second,
/// smaller buffer in which a record the console could not keep up with would be
/// lost.  A short UART write preserves its suffix, so a record that only partly
/// left the port is resumed rather than reprinted.
pub struct DiagnosticDrain {
    /// Ring offset the console has secured: every record before it has been
    /// printed or is held in `pending`, whose bytes are already copied out.
    cursor: u64,
    pending: Option<Queued>,
    offset: usize,
}
impl Default for DiagnosticDrain {
    fn default() -> Self {
        Self::new()
    }
}
impl DiagnosticDrain {
    pub const fn new() -> Self {
        Self {
            cursor: 0,
            pending: None,
            offset: 0,
        }
    }
    pub fn drain_once(&mut self) -> usize {
        self.drain_with(axhal::console::try_write_diagnostic_bytes)
    }
    fn drain_with(&mut self, mut sink: impl FnMut(&[u8]) -> usize) -> usize {
        // At most one record and one finite UART call per turn.
        let Some(mut store) = STORE.try_lock() else {
            return 0;
        };
        if !store.enabled {
            // Console off.  Hold the cursor at the end rather than build a
            // backlog that switching the console back on would replay: a
            // console level control governs what is printed, never what is
            // retained, and it never reprints what it was told to be quiet for.
            self.pending = None;
            self.offset = 0;
            self.cursor = store.end;
            store.console = store.end;
            PENDING.store(false, Ordering::Release);
            return 0;
        }
        while self.pending.is_none() {
            let Some((record, next)) = store.peek(self.cursor) else {
                break;
            };
            self.cursor = next;
            // The record is copied out here, so the console has secured it as
            // far as the ring is concerned even if the UART takes several
            // turns over it.
            store.console = next;
            if record.priority < store.threshold {
                self.pending = Some(record);
            }
        }
        if self.pending.is_none() {
            // Nothing printable is left: the console has secured the whole ring.
            // A cursor behind the oldest retained byte lands here when the only
            // text left is the tail of a record whose head the ring overwrote;
            // those records were counted lost when they were overwritten.
            self.cursor = store.end;
            store.console = store.end;
            PENDING.store(false, Ordering::Release);
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
    /// Most tests build their own `Store`; the ones below drive the real ring,
    /// filter and console, so they take this in case the harness is ever run
    /// with more than one test thread.
    static GLOBAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn global() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL.lock().unwrap_or_else(|error| error.into_inner())
    }
    #[test]
    fn ring_wrap_and_tail_cursor() {
        let mut store = Store::new();
        let mut text = Text::new();
        text.write_str(&"x".repeat(RECORD_BYTES - 1)).unwrap();
        text.finish();
        for _ in 0..70 {
            store.append(&text, 6);
        }
        assert_eq!(store.end, 70 * RECORD_BYTES as u64);
        assert_eq!(store.end - store.oldest, CAPACITY as u64);
        let mut dst = [0; 4];
        let (n, end) = store.snapshot(0, &mut dst, true);
        assert_eq!(n, 4);
        assert_eq!(end, store.end);
        assert_eq!(&dst, b"xxx\n");
    }
    #[test]
    fn concurrent_records_are_not_interleaved() {
        let store = std::sync::Arc::new(SpinNoIrq::new(Store::new()));
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
    #[test]
    fn retention_is_independent_of_console_state() {
        // Console output is a reader, not a copy: turning it off, or filtering
        // what it prints, never costs the retained log a byte.
        let mut store = Store::new();
        let mut text = Text::new();
        text.write_str("message").unwrap();
        text.finish();
        store.enabled = false;
        for _ in 0..80 {
            store.append(&text, 3);
        }
        assert_eq!(store.end, (80 * text.len) as u64);
        assert_eq!(store.console_lost, 0);
        // A level the console would not print is retained all the same.
        store.enabled = true;
        store.threshold = 4;
        store.append(&text, 6);
        assert_eq!(store.end, (81 * text.len) as u64);
        assert_eq!(store.console_lost, 0);
    }
    #[test]
    fn unsupported_and_retired_sink_cannot_be_reenabled() {
        let mut store = Store::new();
        store.supported = false;
        store.set_enabled(true);
        let mut text = Text::new();
        text.write_str("retained").unwrap();
        text.finish();
        store.append(&text, 6);
        assert!(!store.enabled);
        assert_eq!(store.end, text.len as u64);
        store.supported = true;
        store.set_enabled(true);
        store.append(&text, 6);
        assert!(store.enabled);
        store.retire();
        store.set_enabled(true);
        store.append(&text, 6);
        assert!(!store.enabled);
        assert_eq!(store.end, (3 * text.len) as u64);
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
    /// A record is only lost to the console when the ring overwrites it before
    /// the console reads it, and then it is counted, not silent.
    #[test]
    fn the_console_loses_a_record_only_when_the_ring_overwrites_it() {
        let mut store = Store::new();
        let mut text = Text::new();
        text.write_str("twenty-four bytes long..\n").unwrap();
        text.finish();
        let per_record = text.len as u64;
        let records_to_wrap = (CAPACITY as u64 / per_record) as usize;
        for _ in 0..records_to_wrap + 3 {
            store.append(&text, 6);
        }
        // Three records were overwritten while the console cursor never moved.
        assert_eq!(store.console_lost, 3);
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
    /// The reported bug: a burst larger than a bounded console queue reached
    /// the ring and only part of it reached the console, so the serial log
    /// disagreed with `/dev/kmsg` about what the kernel had said.
    #[test]
    fn a_slow_console_prints_every_record_the_ring_retains() {
        let _serial = global();
        const BURST: usize = 200;
        // Other tests log too, so the shared arrival edge may already be set.
        let _ = take_reader_notification();
        set_filter("info").unwrap();
        set_console_enabled(true);
        set_console_threshold(8);
        let before = STORE.lock().end;
        let lost_before = STORE.lock().console_lost;
        for index in 0..BURST {
            diagnostic(format_args!("burst {index:04}"));
        }
        let mut drain = DiagnosticDrain::new();
        drain.cursor = before;
        let mut printed = std::vec::Vec::new();
        // A UART that accepts one byte per turn: far slower than the producer,
        // which is what used to fill the old 64-record console queue.
        let mut turns = 0;
        while diagnostic_work_pending() && turns < 64 * BURST {
            drain.drain_with(|bytes| {
                printed.extend_from_slice(&bytes[..1]);
                1
            });
            turns += 1;
        }
        assert!(!diagnostic_work_pending(), "the console never caught up");
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
            STORE.lock().console_lost,
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
        set_console_enabled(true);
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
        let mut drain = DiagnosticDrain::new();
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
        set_console_enabled(true);
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
            allowed(Level::Info, "kernel"),
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
        set_console_enabled(true);
        set_console_threshold(8);
        let mut drain = DiagnosticDrain::new();
        drain.cursor = STORE.lock().end;
        diagnostic(format_args!("retained"));
        assert!(take_reader_notification());
        assert!(diagnostic_work_pending());
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
        assert!(!diagnostic_work_pending());
        set_filter("warn,test=debug").unwrap();
        assert!(set_filter("info,test=oops").is_err());
        assert_eq!(FILTER.lock().default, LevelFilter::Warn);
        assert_eq!(FILTER.lock().level("test::sub"), LevelFilter::Debug);
        set_filter("info").unwrap();
    }
    /// The console level control governs what is printed, never what is kept,
    /// and switching the console off does not replay what it was quiet for.
    #[test]
    fn console_level_controls_printing_not_retention() {
        let _serial = global();
        // Other tests log too, so the shared arrival edge may already be set.
        let _ = take_reader_notification();
        let mut drain = DiagnosticDrain::new();
        set_console_enabled(true);
        set_console_threshold(8);
        drain.cursor = STORE.lock().end;
        set_console_enabled(false);
        let end_before = STORE.lock().end;
        diagnostic(format_args!("quiet"));
        assert_eq!(STORE.lock().end, end_before + "quiet\n".len() as u64);
        // The record is retained and a console that is off prints none of it.
        // Other tests may have left printable work behind, so this asserts on
        // the sink being untouched rather than on the shared pending flag.
        drain.drain_with(|_| panic!("a disabled console must not touch the UART"));
        set_console_enabled(true);
        diagnostic(format_args!("audible"));
        let mut printed = std::vec::Vec::new();
        drain.drain_with(|bytes| {
            printed.extend_from_slice(bytes);
            bytes.len()
        });
        assert_eq!(printed, b"audible\n");
        set_console_threshold(8);
    }
}
