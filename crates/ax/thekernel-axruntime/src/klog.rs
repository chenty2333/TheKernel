//! Bounded kernel diagnostics. Producers never access a UART or wait for a lock.
use core::{
    fmt::{self, Write},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use kspin::SpinNoIrq;
use log::{Level, LevelFilter, Log, Metadata, Record};

pub const CAPACITY: usize = 64 * 1024;
const RECORD_BYTES: usize = 1024;
const QUEUE_RECORDS: usize = 64;
const FILTERS: usize = 16;
const PREFIX_BYTES: usize = 64;

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
#[derive(Clone, Copy)]
struct Queued {
    text: Text,
    priority: u8,
}
impl Queued {
    const EMPTY: Self = Self {
        text: Text::new(),
        priority: 6,
    };
}
struct Store {
    bytes: [u8; CAPACITY],
    oldest: u64,
    end: u64,
    queue: [Queued; QUEUE_RECORDS],
    head: usize,
    count: usize,
    enabled: bool,
    supported: bool,
    retired: bool,
    threshold: u8,
}
impl Store {
    const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            oldest: 0,
            end: 0,
            queue: [Queued::EMPTY; QUEUE_RECORDS],
            head: 0,
            count: 0,
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
        self.head = 0;
        self.count = 0;
    }
    fn append(&mut self, text: Text, priority: u8) -> bool {
        for byte in text.as_bytes() {
            self.bytes[self.end as usize % CAPACITY] = *byte;
            self.end += 1;
        }
        self.oldest = self.end.saturating_sub(CAPACITY as u64);
        if !self.enabled || priority >= self.threshold {
            return true;
        }
        if self.count == QUEUE_RECORDS {
            return false;
        }
        self.queue[(self.head + self.count) % QUEUE_RECORDS] = Queued { text, priority };
        self.count += 1;
        true
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
    fn pop(&mut self) -> Option<Queued> {
        if self.count == 0 {
            return None;
        }
        let record = self.queue[self.head];
        self.head = (self.head + 1) % QUEUE_RECORDS;
        self.count -= 1;
        Some(record)
    }
}
static STORE: SpinNoIrq<Store> = SpinNoIrq::new(Store::new());
static PENDING: AtomicBool = AtomicBool::new(false);
static LOST_RECORDS: AtomicU64 = AtomicU64::new(0);
static LOST_DIAGNOSTICS: AtomicU64 = AtomicU64::new(0);
static TRUNCATED: AtomicU64 = AtomicU64::new(0);
static PRODUCING: [AtomicBool; axconfig::plat::MAX_CPU_NUM] =
    [const { AtomicBool::new(false) }; axconfig::plat::MAX_CPU_NUM];

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
    let (overwritten, supported, retired) = {
        let store = STORE.lock();
        (store.oldest, store.supported, store.retired)
    };
    writeln!(
        out,
        "records_dropped {}\ndiagnostic_records_dropped {}\nrecords_truncated \
         {}\nretention_bytes_overwritten {}\ndiagnostic_supported {}\ndiagnostic_retired {}",
        LOST_RECORDS.load(Ordering::Relaxed),
        LOST_DIAGNOSTICS.load(Ordering::Relaxed),
        TRUNCATED.load(Ordering::Relaxed),
        overwritten,
        supported as u8,
        retired as u8
    )
}
fn allowed(level: Level, target: &str) -> bool {
    let Some(filter) = FILTER.try_lock() else {
        LOST_RECORDS.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    level <= filter.level(target)
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
        PRODUCING[self.cpu].store(false, Ordering::Release);
    }
}
fn producer_guard() -> Option<ProducerGuard> {
    // Pin the producer before selecting its CPU-local recursion slot. IRQs
    // remain enabled; a nested IRQ producer sees the bit and drops promptly.
    let preempt = kernel_guard::NoPreempt::new();
    #[cfg(not(test))]
    let cpu = axhal::percpu::this_cpu_id();
    #[cfg(test)]
    let cpu = 0;
    if PRODUCING[cpu].swap(true, Ordering::Acquire) {
        LOST_RECORDS.fetch_add(1, Ordering::Relaxed);
        None
    } else {
        Some(ProducerGuard {
            cpu,
            _preempt: preempt,
        })
    }
}
fn append(text: Text, level: Level) {
    let Some(mut store) = STORE.try_lock() else {
        LOST_RECORDS.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if text.truncated {
        TRUNCATED.fetch_add(1, Ordering::Relaxed);
    }
    if !store.append(text, priority(level)) {
        LOST_DIAGNOSTICS.fetch_add(1, Ordering::Relaxed);
    }
    // Publish under the same lock as consumer's empty check; no missed wake.
    if store.count != 0 {
        PENDING.store(true, Ordering::Release);
    }
}
struct Logger;
impl Log for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        allowed(metadata.level(), metadata.target())
    }
    fn log(&self, record: &Record<'_>) {
        let Some(_guard) = producer_guard() else {
            return;
        };
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
        append(text, record.level());
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
    let Some(_guard) = producer_guard() else {
        return;
    };
    let mut text = Text::new();
    let _ = text.write_fmt(args);
    text.finish();
    append(text, Level::Info);
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
pub fn diagnostic_work_pending() -> bool {
    PENDING.load(Ordering::Acquire)
}

/// A single task owns this cursor. A short UART write preserves its suffix.
pub struct DiagnosticDrain {
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
        if self.pending.is_none() {
            self.pending = store.pop();
            self.offset = 0;
        }
        if let Some(record) = self.pending.as_ref() {
            if !store.enabled || record.priority >= store.threshold {
                self.pending = None;
            }
        }
        if self.pending.is_none() && store.count == 0 {
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
    #[test]
    fn ring_wrap_and_tail_cursor() {
        let mut store = Store::new();
        let mut text = Text::new();
        text.write_str(&"x".repeat(RECORD_BYTES - 1)).unwrap();
        text.finish();
        for _ in 0..70 {
            store.append(text, 6);
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
                    store.lock().append(text, 6);
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
    fn retention_independent_of_diagnostic_capacity_and_threshold() {
        let mut store = Store::new();
        let mut text = Text::new();
        text.write_str("message").unwrap();
        text.finish();
        for _ in 0..QUEUE_RECORDS {
            assert!(store.append(text, 6));
        }
        assert!(!store.append(text, 6));
        assert_eq!(store.end, ((QUEUE_RECORDS + 1) * text.len) as u64);
        store.enabled = false;
        assert!(store.append(text, 3));
        assert_eq!(store.count, QUEUE_RECORDS);
    }
    #[test]
    fn unsupported_and_retired_sink_cannot_be_reenabled() {
        let mut store = Store::new();
        store.supported = false;
        store.set_enabled(true);
        let mut text = Text::new();
        text.write_str("retained").unwrap();
        text.finish();
        store.append(text, 6);
        assert_eq!(store.count, 0);
        assert_eq!(store.end, text.len as u64);
        store.supported = true;
        store.set_enabled(true);
        store.append(text, 6);
        assert_eq!(store.count, 1);
        store.retire();
        store.set_enabled(true);
        store.append(text, 6);
        assert!(!store.enabled);
        assert_eq!(store.count, 0);
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
    #[test]
    fn producer_contention_reentrancy_and_partial_drain() {
        // All other tests use local stores, so this owns the global test state.
        let before = LOST_RECORDS.load(Ordering::Relaxed);
        {
            let store = STORE.lock();
            diagnostic(format_args!("contended"));
            assert_eq!(store.end, 0);
        }
        assert_eq!(LOST_RECORDS.load(Ordering::Relaxed), before + 1);
        {
            let _guard = producer_guard().unwrap();
            diagnostic(format_args!("recursive"));
        }
        assert_eq!(LOST_RECORDS.load(Ordering::Relaxed), before + 2);
        diagnostic(format_args!("retained"));
        assert_eq!(STORE.lock().count, 1);
        let mut drain = DiagnosticDrain::new();
        let mut retained = std::vec::Vec::new();
        drain.drain_with(|bytes| {
            retained.extend_from_slice(bytes);
            bytes.len()
        });
        assert_eq!(retained, b"retained\n");
        drain.drain_with(|_| panic!("empty queue must not touch UART"));
        assert!(!diagnostic_work_pending());
        set_filter("warn,test=debug").unwrap();
        assert!(set_filter("info,test=oops").is_err());
        assert_eq!(FILTER.lock().default, LevelFilter::Warn);
        assert_eq!(FILTER.lock().level("test::sub"), LevelFilter::Debug);
        let mut text = Text::new();
        text.write_str("abcdef").unwrap();
        text.finish();
        let mut drain = DiagnosticDrain {
            pending: Some(Queued { text, priority: 6 }),
            offset: 0,
        };
        let mut output = std::vec::Vec::new();
        drain.drain_with(|_| 0);
        assert_eq!(drain.offset, 0);
        drain.drain_with(|bytes| {
            output.extend_from_slice(&bytes[..2]);
            2
        });
        assert_eq!(drain.offset, 2);
        drain.drain_with(|bytes| {
            output.extend_from_slice(bytes);
            bytes.len()
        });
        assert_eq!(output, b"abcdef\n");
        assert!(drain.pending.is_none());
    }
}
