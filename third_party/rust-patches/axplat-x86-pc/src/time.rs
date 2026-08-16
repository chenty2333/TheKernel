//! Time management for the x86 platform.
//!
//! The platform clock is selected at boot.  An invariant TSC is preferred; on
//! machines that do not advertise one, a 64-bit HPET main counter is the only
//! accepted long-lived fallback.  The LAPIC timer is calibrated independently
//! when the CPU does not provide TSC-deadline mode.

#[cfg(feature = "irq")]
use core::sync::atomic::AtomicBool;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use axplat::{
    mem::{PhysAddr, pa, phys_to_virt},
    time::TimeIf,
};
use kspin::SpinNoIrq;
use raw_cpuid::CpuId;
#[cfg(feature = "irq")]
use x2apic::lapic::{LocalApic, TimerDivide, TimerMode};
#[cfg(feature = "irq")]
use x86::msr::{IA32_TSC_DEADLINE, wrmsr};
use x86_64::instructions::port::Port;

const NANOS_PER_SEC: u64 = 1_000_000_000;
const FEMTOS_PER_SEC: u64 = 1_000_000_000_000_000;

// These are validation bounds, not clock configuration.  A TSC or reference
// timer outside these bounds is treated as an unusable hardware report.
const MIN_CLOCK_FREQUENCY_HZ: u64 = 1_000_000;
const MAX_CLOCK_FREQUENCY_HZ: u64 = 100_000_000_000;
#[cfg(feature = "irq")]
const MAX_LAPIC_FREQUENCY_HZ: u64 = 20_000_000_000;

const HPET_BASE: PhysAddr = pa!(0xfed0_0000);
const HPET_GENERAL_CAPABILITIES: usize = 0x00;
const HPET_GENERAL_CONFIGURATION: usize = 0x10;
const HPET_MAIN_COUNTER: usize = 0xf0;
const HPET_COUNTER_64_BIT: u64 = 1 << 13;
const HPET_ENABLE: u64 = 1;
const HPET_NOMINAL_PERIOD_MIN_FS: u64 = 1_000_000;
const HPET_NOMINAL_PERIOD_MAX_FS: u64 = 100_000_000;

// Ten milliseconds is long enough to make MMIO/reference-clock quantization
// insignificant while keeping early boot bounded.
const CALIBRATION_WINDOW_NS: u64 = 10_000_000;
const HPET_MAX_POLLS: usize = 20_000_000;
const PIT_MAX_POLLS: usize = 20_000_000;

// The PIT input clock is a hardware-defined reference, not a platform
// configuration value.  Channel 2 with divisor 0 counts 65536 periods.
const PIT_FREQUENCY_HZ: u64 = 1_193_182;
const PIT_DIVISOR: u64 = 1 << 16;

static INIT_TICK: AtomicU64 = AtomicU64::new(0);
static CLOCK_FREQUENCY_HZ: AtomicU64 = AtomicU64::new(0);
static CLOCK_SOURCE: AtomicU8 = AtomicU8::new(ClockSource::Uninitialized as u8);
static RTC_EPOCHOFFSET_NANOS: AtomicU64 = AtomicU64::new(0);

// HPET and PIT are process-wide devices.  APs enter init_later_secondary
// independently, so the complete reference-counter/LAPIC calibration must be
// serialized, including restoration of the reference-device state.  SpinNoIrq
// disables local interrupts while waiting; calibration is bounded polling and
// never sleeps in an interrupt context.
static CALIBRATION_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());

#[cfg(feature = "irq")]
static TSC_DEADLINE_MODE: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "irq")]
static LAPIC_FREQUENCY_HZ: [AtomicU64; crate::config::plat::MAX_CPU_NUM] =
    [const { AtomicU64::new(0) }; crate::config::plat::MAX_CPU_NUM];

/// Where a frequency sample came from.  This is also printed during boot so
/// a machine without a direct CPUID report does not look as if it used a
/// configuration constant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrequencySource {
    Cpuid15,
    Hpet,
    Pit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum ClockSource {
    Uninitialized = 0,
    Tsc           = 1,
    Hpet          = 2,
}

impl ClockSource {
    fn load() -> Self {
        match CLOCK_SOURCE.load(Ordering::Acquire) {
            value if value == Self::Tsc as u8 => Self::Tsc,
            value if value == Self::Hpet as u8 => Self::Hpet,
            _ => Self::Uninitialized,
        }
    }

    fn store(self) {
        CLOCK_SOURCE.store(self as u8, Ordering::Release);
    }
}

fn select_clock_source(
    invariant_tsc: bool,
    tsc_calibrated: bool,
    hpet_counter_64: bool,
) -> Option<ClockSource> {
    if invariant_tsc && tsc_calibrated {
        Some(ClockSource::Tsc)
    } else if hpet_counter_64 {
        Some(ClockSource::Hpet)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrequencySample {
    hz: u64,
    source: FrequencySource,
}

fn valid_frequency(hz: u64, maximum: u64) -> bool {
    (MIN_CLOCK_FREQUENCY_HZ..=maximum).contains(&hz)
}

/// Scale a counter delta by a reference frequency without overflowing a
/// 64-bit intermediate.  Saturating at the API boundary is preferable to a
/// wrapped clock, which would make every later deadline invalid.
fn frequency_from_counts(
    counter_delta: u64,
    reference_delta: u64,
    reference_hz: u64,
) -> Option<u64> {
    if counter_delta == 0 || reference_delta == 0 || reference_hz == 0 {
        return None;
    }
    let hz = u128::from(counter_delta).checked_mul(u128::from(reference_hz))?
        / u128::from(reference_delta);
    u64::try_from(hz).ok()
}

/// Convert a hardware tick count to nanoseconds with a saturating u128
/// intermediate.  This helper is intentionally pure so large-counter cases
/// can be tested without booting a platform.
fn ticks_to_nanos_checked(ticks: u64, frequency_hz: u64) -> u64 {
    if frequency_hz == 0 {
        return 0;
    }
    let nanos =
        u128::from(ticks).saturating_mul(u128::from(NANOS_PER_SEC)) / u128::from(frequency_hz);
    nanos.min(u128::from(u64::MAX)) as u64
}

/// Convert nanoseconds to hardware ticks with a saturating u128
/// intermediate.  Saturation keeps a far-future deadline from becoming a
/// near-past deadline after a 64-bit multiplication wraps.
fn nanos_to_ticks_checked(nanos: u64, frequency_hz: u64) -> u64 {
    if frequency_hz == 0 {
        return 0;
    }
    let ticks =
        u128::from(nanos).saturating_mul(u128::from(frequency_hz)) / u128::from(NANOS_PER_SEC);
    ticks.min(u128::from(u64::MAX)) as u64
}

#[cfg(feature = "irq")]
fn lapic_ticks_for_nanos(nanos: u64, frequency_hz: u64) -> u32 {
    nanos_to_ticks_checked(nanos, frequency_hz).clamp(1, u64::from(u32::MAX)) as u32
}

fn cpuid15_frequency(denominator: u32, numerator: u32, crystal_hz: u32) -> Option<u64> {
    if denominator == 0 || numerator == 0 || crystal_hz == 0 {
        return None;
    }
    let hz = u128::from(crystal_hz).checked_mul(u128::from(numerator))? / u128::from(denominator);
    let hz = u64::try_from(hz).ok()?;
    valid_frequency(hz, MAX_CLOCK_FREQUENCY_HZ).then_some(hz)
}

fn cpuid15_frequency_from_host() -> Option<u64> {
    CpuId::new().get_tsc_info().and_then(|info| {
        cpuid15_frequency(
            info.denominator(),
            info.numerator(),
            info.nominal_frequency(),
        )
    })
}

/// Select the most trustworthy available TSC sample.  CPUID leaf 0x15 is a
/// direct TSC/crystal ratio.  A measured HPET/PIT interval is next.  CPUID
/// leaf 0x16 is deliberately not accepted: it describes processor frequency,
/// not a TSC ratio.
fn select_tsc_frequency(
    cpuid15_hz: Option<u64>,
    hpet_hz: Option<u64>,
    pit_hz: Option<u64>,
) -> Option<FrequencySample> {
    [
        (cpuid15_hz, FrequencySource::Cpuid15),
        (hpet_hz, FrequencySource::Hpet),
        (pit_hz, FrequencySource::Pit),
    ]
    .into_iter()
    .find_map(|(hz, source)| {
        hz.filter(|hz| valid_frequency(*hz, MAX_CLOCK_FREQUENCY_HZ))
            .map(|hz| FrequencySample { hz, source })
    })
}

fn invariant_tsc_available() -> bool {
    let cpuid = CpuId::new();
    cpuid
        .get_feature_info()
        .is_some_and(|features| features.has_tsc())
        && cpuid
            .get_advanced_power_mgmt_info()
            .is_some_and(|features| features.has_invariant_tsc())
}

#[inline]
fn read_tsc() -> u64 {
    // LFENCE orders the read from the surrounding MMIO and APIC operations.
    // This is sufficient for interval sampling and avoids requiring RDTSCP on
    // every x86_64 CPU accepted by the platform.
    unsafe { core::arch::x86_64::_mm_lfence() };
    let value = unsafe { core::arch::x86_64::_rdtsc() };
    unsafe { core::arch::x86_64::_mm_lfence() };
    value
}

#[derive(Clone, Copy)]
struct Hpet {
    base: *mut u8,
    period_fs: u64,
    counter_64_bit: bool,
}

impl Hpet {
    fn new() -> Option<Self> {
        let base = phys_to_virt(HPET_BASE).as_mut_ptr();
        let capabilities =
            unsafe { core::ptr::read_volatile(base.add(HPET_GENERAL_CAPABILITIES) as *const u64) };
        let period_fs = capabilities >> 32;
        if !(HPET_NOMINAL_PERIOD_MIN_FS..=HPET_NOMINAL_PERIOD_MAX_FS).contains(&period_fs) {
            return None;
        }
        Some(Self {
            base,
            period_fs,
            counter_64_bit: capabilities & HPET_COUNTER_64_BIT != 0,
        })
    }

    unsafe fn read_u64(&self, offset: usize) -> u64 {
        unsafe { core::ptr::read_volatile(self.base.add(offset) as *const u64) }
    }

    unsafe fn read_u32(&self, offset: usize) -> u32 {
        unsafe { core::ptr::read_volatile(self.base.add(offset) as *const u32) }
    }

    unsafe fn write_u64(&self, offset: usize, value: u64) {
        unsafe { core::ptr::write_volatile(self.base.add(offset) as *mut u64, value) }
    }

    unsafe fn configuration(&self) -> u64 {
        unsafe { self.read_u64(HPET_GENERAL_CONFIGURATION) }
    }

    unsafe fn set_configuration(&self, value: u64) {
        unsafe { self.write_u64(HPET_GENERAL_CONFIGURATION, value) }
    }

    unsafe fn counter(&self) -> u64 {
        if self.counter_64_bit {
            unsafe { self.read_u64(HPET_MAIN_COUNTER) }
        } else {
            unsafe { u64::from(self.read_u32(HPET_MAIN_COUNTER)) }
        }
    }

    fn frequency_hz(&self) -> Option<u64> {
        let hz = u128::from(FEMTOS_PER_SEC) / u128::from(self.period_fs);
        let hz = u64::try_from(hz).ok()?;
        valid_frequency(hz, MAX_CLOCK_FREQUENCY_HZ).then_some(hz)
    }
}

/// Read the selected HPET clocksource without reprobling its capabilities on
/// every time query.  This is called only after `hpet_clocksource_sample`
/// verified that the counter is 64-bit and left the block enabled.
#[inline]
fn read_hpet_clock_counter() -> u64 {
    let base = phys_to_virt(HPET_BASE).as_mut_ptr();
    unsafe { core::ptr::read_volatile(base.add(HPET_MAIN_COUNTER) as *const u64) }
}

fn hpet_clocksource_sample() -> Option<FrequencySample> {
    let hpet = Hpet::new()?;
    if !hpet.counter_64_bit {
        // A 32-bit HPET wraps too quickly for a long-lived monotonic source;
        // it remains usable as a short calibration reference only.
        return None;
    }
    let frequency_hz = hpet.frequency_hz()?;
    let old_configuration = unsafe { hpet.configuration() };
    unsafe { hpet.set_configuration(old_configuration | HPET_ENABLE) };
    let start = unsafe { hpet.counter() };
    if unsafe { wait_hpet(&hpet, start, 1) }.is_none() {
        // Do not publish a clocksource whose main counter does not advance.
        unsafe { hpet.set_configuration(old_configuration) };
        return None;
    }
    Some(FrequencySample {
        hz: frequency_hz,
        source: FrequencySource::Hpet,
    })
}

fn counter_elapsed(start: u64, end: u64, counter_64_bit: bool) -> u64 {
    if counter_64_bit {
        end.wrapping_sub(start)
    } else {
        u64::from((end as u32).wrapping_sub(start as u32))
    }
}

fn hpet_ticks_for_nanos(period_fs: u64, nanos: u64) -> Option<u64> {
    if period_fs == 0 {
        return None;
    }
    let ticks = u128::from(nanos).checked_mul(u128::from(FEMTOS_PER_SEC))?
        / (u128::from(period_fs) * u128::from(NANOS_PER_SEC));
    u64::try_from(ticks.max(1)).ok()
}

#[cfg(feature = "irq")]
fn absolute_deadline_ticks(now: u64, delta: u64) -> u64 {
    // A wrapped absolute deadline would be interpreted as already elapsed by
    // the LAPIC.  Saturation preserves the far-future ordering instead.
    now.saturating_add(delta)
}

unsafe fn wait_hpet(hpet: &Hpet, start: u64, target_ticks: u64) -> Option<u64> {
    for _ in 0..HPET_MAX_POLLS {
        let now = unsafe { hpet.counter() };
        if counter_elapsed(start, now, hpet.counter_64_bit) >= target_ticks {
            return Some(now);
        }
    }
    None
}

fn calibrate_tsc_with_hpet() -> Option<u64> {
    let hpet = Hpet::new()?;
    let target_ticks = hpet_ticks_for_nanos(hpet.period_fs, CALIBRATION_WINDOW_NS)?;
    let reference_hz = hpet.frequency_hz()?;

    let old_configuration = unsafe { hpet.configuration() };
    unsafe {
        hpet.set_configuration(old_configuration | HPET_ENABLE);
    }
    let reference_start = unsafe { hpet.counter() };
    let tsc_start = read_tsc();
    let reference_end = unsafe { wait_hpet(&hpet, reference_start, target_ticks) };
    let tsc_end = read_tsc();
    unsafe { hpet.set_configuration(old_configuration) };

    let reference_end = reference_end?;
    let reference_delta = counter_elapsed(reference_start, reference_end, hpet.counter_64_bit);
    frequency_from_counts(
        tsc_end.wrapping_sub(tsc_start),
        reference_delta,
        reference_hz,
    )
    .filter(|hz| valid_frequency(*hz, MAX_CLOCK_FREQUENCY_HZ))
}

/// Program PIT channel 2 in one-shot mode and wait for its speaker output.
/// This function is only called at early boot, before device drivers own the
/// PIT/speaker ports.
unsafe fn calibrate_tsc_with_pit() -> Option<u64> {
    let mut speaker = Port::<u8>::new(0x61);
    let old_speaker = unsafe { speaker.read() };
    unsafe { speaker.write(old_speaker | 1) };

    let tsc_start = read_tsc();
    let mut command = Port::<u8>::new(0x43);
    let mut channel_2 = Port::<u8>::new(0x42);
    unsafe {
        command.write(0xb0); // channel 2, lobyte/hibyte, mode 0, binary
        channel_2.write(0xff);
        channel_2.write(0xff);
    }

    let mut completed = false;
    for _ in 0..PIT_MAX_POLLS {
        if unsafe { speaker.read() } & 0x20 != 0 {
            completed = true;
            break;
        }
    }
    let tsc_end = read_tsc();
    unsafe { speaker.write(old_speaker) };

    if !completed {
        return None;
    }
    frequency_from_counts(
        tsc_end.wrapping_sub(tsc_start),
        PIT_DIVISOR,
        PIT_FREQUENCY_HZ,
    )
    .filter(|hz| valid_frequency(*hz, MAX_CLOCK_FREQUENCY_HZ))
}

fn tsc_frequency_sample() -> Option<FrequencySample> {
    let cpuid15_hz = cpuid15_frequency_from_host();
    let hpet_hz = if cpuid15_hz.is_none() {
        calibrate_tsc_with_hpet()
    } else {
        None
    };
    let pit_hz = if cpuid15_hz.is_none() && hpet_hz.is_none() {
        unsafe { calibrate_tsc_with_pit() }
    } else {
        None
    };
    select_tsc_frequency(cpuid15_hz, hpet_hz, pit_hz)
}

fn select_clock_at_boot(invariant_tsc: bool) -> Option<(ClockSource, FrequencySample)> {
    // Keep the lock across the complete calibration and any HPET state change.
    // `SpinNoIrq` makes this safe during early boot and AP startup, where a
    // sleeping mutex is not available and interrupts must not preempt polling.
    let _guard = CALIBRATION_LOCK.lock();
    let tsc_sample = invariant_tsc.then(tsc_frequency_sample).flatten();
    let hpet_sample = if tsc_sample.is_none() {
        hpet_clocksource_sample()
    } else {
        None
    };
    match select_clock_source(invariant_tsc, tsc_sample.is_some(), hpet_sample.is_some())? {
        ClockSource::Tsc => Some((ClockSource::Tsc, tsc_sample?)),
        ClockSource::Hpet => Some((ClockSource::Hpet, hpet_sample?)),
        ClockSource::Uninitialized => None,
    }
}

pub fn init_early() {
    let invariant_tsc = invariant_tsc_available();
    let (clock_source, sample) = select_clock_at_boot(invariant_tsc).unwrap_or_else(|| {
        if invariant_tsc {
            panic!(
                "unable to calibrate invariant TSC: CPUID 0x15, HPET, and PIT provided no usable \
                 frequency, and no 64-bit HPET clocksource is available"
            );
        }
        panic!("x86 has no invariant TSC and no usable 64-bit HPET monotonic clocksource");
    });
    CLOCK_FREQUENCY_HZ.store(sample.hz, Ordering::Release);
    clock_source.store();

    let init_tick = match clock_source {
        ClockSource::Tsc => read_tsc(),
        ClockSource::Hpet => read_hpet_clock_counter(),
        ClockSource::Uninitialized => unreachable!(),
    };
    INIT_TICK.store(init_tick, Ordering::Release);
    axplat::console_println!(
        "clocksource: {:?}, frequency: {} Hz (calibrated by {:?})",
        clock_source,
        CLOCK_FREQUENCY_HZ.load(Ordering::Acquire),
        sample.source
    );

    #[cfg(feature = "rtc")]
    {
        use x86_rtc::Rtc;

        // Get the current time in microseconds since the epoch (1970-01-01)
        // from the x86 RTC.  Saturation keeps a malformed RTC value from
        // wrapping the wall-clock offset.
        let epoch_nanos = Rtc::new()
            .get_unix_timestamp()
            .saturating_mul(NANOS_PER_SEC);
        // `current_ticks()` is measured from INIT_TICK, so the monotonic
        // clock starts at zero.  The RTC value itself is therefore the wall
        // clock offset; subtracting the raw INIT_TICK would double-count the
        // boot origin.
        RTC_EPOCHOFFSET_NANOS.store(epoch_nanos, Ordering::Release);
    }
}

#[cfg(feature = "irq")]
fn current_cpu_lapic_frequency() -> u64 {
    let cpu = crate::cpu::current_logical_cpu_id();
    LAPIC_FREQUENCY_HZ
        .get(cpu)
        .map(|frequency| frequency.load(Ordering::Acquire))
        .unwrap_or(0)
}

#[cfg(feature = "irq")]
unsafe fn prepare_lapic_for_calibration(lapic: &mut LocalApic) {
    unsafe {
        lapic.disable_timer();
        lapic.set_timer_mode(TimerMode::OneShot);
        lapic.set_timer_divide(TimerDivide::Div1);
        lapic.set_timer_initial(u32::MAX);
        // The LVT mask blocks delivery, but the x2APIC implementation also
        // treats the disabled state as stopped for this calibration path.
        // Unmask only while SpinNoIrq holds the CPU with interrupts disabled;
        // this starts the countdown without allowing a calibration IRQ to
        // preempt the polling loop.
        lapic.enable_timer();
    }
}

#[cfg(feature = "irq")]
unsafe fn finish_lapic_calibration(lapic: &mut LocalApic) -> u64 {
    let elapsed = u64::from(u32::MAX.wrapping_sub(unsafe { lapic.timer_current() }));
    unsafe {
        lapic.disable_timer();
        lapic.set_timer_initial(0);
    }
    elapsed
}

#[cfg(feature = "irq")]
unsafe fn calibrate_lapic_with_hpet(lapic: &mut LocalApic) -> Option<u64> {
    let hpet = Hpet::new()?;
    let target_ticks = hpet_ticks_for_nanos(hpet.period_fs, CALIBRATION_WINDOW_NS)?;
    let reference_hz = hpet.frequency_hz()?;
    let old_configuration = unsafe { hpet.configuration() };
    unsafe { hpet.set_configuration(old_configuration | HPET_ENABLE) };

    unsafe { prepare_lapic_for_calibration(lapic) };
    let reference_start = unsafe { hpet.counter() };
    let reference_end = unsafe { wait_hpet(&hpet, reference_start, target_ticks) };
    let lapic_delta = unsafe { finish_lapic_calibration(lapic) };
    unsafe { hpet.set_configuration(old_configuration) };

    let reference_end = reference_end?;
    let reference_delta = counter_elapsed(reference_start, reference_end, hpet.counter_64_bit);
    frequency_from_counts(lapic_delta, reference_delta, reference_hz)
        .filter(|hz| valid_frequency(*hz, MAX_LAPIC_FREQUENCY_HZ))
}

#[cfg(feature = "irq")]
unsafe fn calibrate_lapic_with_pit(lapic: &mut LocalApic) -> Option<u64> {
    let mut speaker = Port::<u8>::new(0x61);
    let old_speaker = unsafe { speaker.read() };
    unsafe { speaker.write(old_speaker | 1) };

    let mut command = Port::<u8>::new(0x43);
    let mut channel_2 = Port::<u8>::new(0x42);
    unsafe { prepare_lapic_for_calibration(lapic) };
    unsafe {
        command.write(0xb0);
        channel_2.write(0xff);
        channel_2.write(0xff);
    }

    let mut completed = false;
    for _ in 0..PIT_MAX_POLLS {
        if unsafe { speaker.read() } & 0x20 != 0 {
            completed = true;
            break;
        }
    }
    let lapic_delta = unsafe { finish_lapic_calibration(lapic) };
    unsafe { speaker.write(old_speaker) };

    if !completed {
        return None;
    }
    frequency_from_counts(lapic_delta, PIT_DIVISOR, PIT_FREQUENCY_HZ)
        .filter(|hz| valid_frequency(*hz, MAX_LAPIC_FREQUENCY_HZ))
}

#[cfg(feature = "irq")]
fn calibrate_lapic_frequency(lapic: &mut LocalApic) -> Option<(u64, FrequencySource)> {
    let _guard = CALIBRATION_LOCK.lock();
    // HPET is a memory-mapped reference with much better resolution than the
    // PIT.  Use PIT only when HPET is absent or malformed.
    unsafe { calibrate_lapic_with_hpet(lapic) }
        .map(|hz| (hz, FrequencySource::Hpet))
        .or_else(|| unsafe { calibrate_lapic_with_pit(lapic) }.map(|hz| (hz, FrequencySource::Pit)))
}

#[cfg(feature = "irq")]
fn store_lapic_frequency(frequency: Option<(u64, FrequencySource)>) {
    let cpu = crate::cpu::current_logical_cpu_id();
    let (frequency_hz, source) = frequency.unwrap_or_else(|| {
        panic!("unable to calibrate LAPIC timer on logical CPU {cpu}: HPET and PIT unavailable")
    });
    LAPIC_FREQUENCY_HZ[cpu].store(frequency_hz, Ordering::Release);
    axplat::console_println!(
        "LAPIC timer frequency on CPU {}: {} Hz ({:?})",
        cpu,
        frequency_hz,
        source
    );
}

pub fn init_primary() {
    #[cfg(feature = "irq")]
    unsafe {
        let lapic = super::apic::local_apic();
        let supports_tsc_deadline = ClockSource::load() == ClockSource::Tsc
            && CpuId::new()
                .get_feature_info()
                .is_some_and(|features| features.has_tsc_deadline());
        TSC_DEADLINE_MODE.store(supports_tsc_deadline, Ordering::Release);

        if supports_tsc_deadline {
            // TSC-deadline mode compares an absolute TSC value and therefore
            // uses the calibrated TSC frequency directly; the LAPIC divider
            // is irrelevant in this mode.
            lapic.set_timer_mode(TimerMode::TscDeadline);
            lapic.set_timer_divide(TimerDivide::Div1);
            lapic.enable_timer();
            axplat::console_println!("LAPIC timer mode: TSC-deadline");
        } else {
            // Legacy one-shot mode counts the local APIC bus timer.  Never
            // derive that frequency from the TSC or a configuration default.
            store_lapic_frequency(calibrate_lapic_frequency(lapic));
            lapic.set_timer_mode(TimerMode::OneShot);
            lapic.set_timer_divide(TimerDivide::Div1);
            lapic.enable_timer();
        }
    }
}

#[cfg(feature = "smp")]
pub fn init_secondary() {
    #[cfg(feature = "irq")]
    unsafe {
        let lapic = crate::apic::local_apic();
        if TSC_DEADLINE_MODE.load(Ordering::Acquire) {
            lapic.set_timer_mode(TimerMode::TscDeadline);
            lapic.set_timer_divide(TimerDivide::Div1);
        } else {
            store_lapic_frequency(calibrate_lapic_frequency(lapic));
            lapic.set_timer_mode(TimerMode::OneShot);
            lapic.set_timer_divide(TimerDivide::Div1);
        }
        lapic.enable_timer();
    }
}

struct TimeIfImpl;

#[inline]
fn current_clock_counter() -> u64 {
    match ClockSource::load() {
        ClockSource::Tsc => read_tsc(),
        ClockSource::Hpet => read_hpet_clock_counter(),
        ClockSource::Uninitialized => panic!("clocksource queried before x86 time initialization"),
    }
}

#[impl_plat_interface]
impl TimeIf for TimeIfImpl {
    /// Returns the current clock time in hardware ticks.
    fn current_ticks() -> u64 {
        current_clock_counter().wrapping_sub(INIT_TICK.load(Ordering::Acquire))
    }

    /// Converts hardware ticks to nanoseconds.
    fn ticks_to_nanos(ticks: u64) -> u64 {
        ticks_to_nanos_checked(ticks, CLOCK_FREQUENCY_HZ.load(Ordering::Acquire))
    }

    /// Converts nanoseconds to hardware ticks.
    fn nanos_to_ticks(nanos: u64) -> u64 {
        nanos_to_ticks_checked(nanos, CLOCK_FREQUENCY_HZ.load(Ordering::Acquire))
    }

    /// Return epoch offset in nanoseconds (wall time offset to monotonic
    /// clock start).
    fn epochoffset_nanos() -> u64 {
        RTC_EPOCHOFFSET_NANOS.load(Ordering::Acquire)
    }

    /// Returns the IRQ number for the timer interrupt.
    #[cfg(feature = "irq")]
    fn irq_num() -> usize {
        crate::config::devices::TIMER_IRQ
    }

    /// Set a one-shot timer.
    ///
    /// TSC-deadline mode receives an absolute TSC deadline.  Older LAPICs use
    /// a locally calibrated countdown; both paths use measured frequencies.
    #[cfg(feature = "irq")]
    fn set_oneshot_timer(deadline_ns: u64) {
        if TSC_DEADLINE_MODE.load(Ordering::Acquire) {
            let now_tsc = read_tsc();
            let now_ns = Self::ticks_to_nanos(Self::current_ticks());
            let delta_ns = deadline_ns.saturating_sub(now_ns);
            let delta_ticks = Self::nanos_to_ticks(delta_ns).max(1);
            let deadline_tsc = absolute_deadline_ticks(now_tsc, delta_ticks);
            unsafe { wrmsr(IA32_TSC_DEADLINE, deadline_tsc) };
            return;
        }

        let lapic_hz = current_cpu_lapic_frequency();
        assert!(lapic_hz != 0, "LAPIC timer frequency was not calibrated");
        let now_ns = Self::ticks_to_nanos(Self::current_ticks());
        let delta_ns = deadline_ns.saturating_sub(now_ns);
        let lapic_ticks = lapic_ticks_for_nanos(delta_ns, lapic_hz);
        unsafe {
            super::apic::local_apic().set_timer_initial(lapic_ticks);
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "irq")]
    use super::absolute_deadline_ticks;
    #[cfg(feature = "irq")]
    use super::lapic_ticks_for_nanos;
    use super::{
        ClockSource, FrequencySample, FrequencySource, counter_elapsed, cpuid15_frequency,
        frequency_from_counts, hpet_ticks_for_nanos, nanos_to_ticks_checked, select_clock_source,
        select_tsc_frequency, ticks_to_nanos_checked,
    };

    #[test]
    fn cpuid_15_frequency_uses_ratio_and_rejects_empty_fields() {
        assert_eq!(cpuid15_frequency(1, 100, 24_000_000), Some(2_400_000_000));
        assert_eq!(cpuid15_frequency(0, 100, 24_000_000), None);
        assert_eq!(cpuid15_frequency(1, 0, 24_000_000), None);
        assert_eq!(cpuid15_frequency(1, 100, 0), None);
    }

    #[test]
    fn calibration_source_priority_is_explicit() {
        assert_eq!(
            select_tsc_frequency(
                Some(3_600_000_000),
                Some(3_500_000_000),
                Some(3_400_000_000),
            ),
            Some(FrequencySample {
                hz: 3_600_000_000,
                source: FrequencySource::Cpuid15,
            })
        );
        assert_eq!(
            select_tsc_frequency(None, Some(3_500_000_000), None).map(|sample| sample.source),
            Some(FrequencySource::Hpet)
        );
        assert_eq!(
            select_tsc_frequency(None, None, Some(3_400_000_000)).map(|sample| sample.source),
            Some(FrequencySource::Pit)
        );
        assert_eq!(select_tsc_frequency(None, None, None), None);
    }

    #[test]
    fn clocksource_state_requires_invariant_tsc_or_64_bit_hpet() {
        assert_eq!(
            select_clock_source(true, true, false),
            Some(ClockSource::Tsc)
        );
        assert_eq!(
            select_clock_source(false, false, true),
            Some(ClockSource::Hpet)
        );
        assert_eq!(select_clock_source(false, false, false), None);
        assert_eq!(select_clock_source(true, false, false), None);
    }

    #[cfg(feature = "irq")]
    #[test]
    fn absolute_deadlines_do_not_wrap_into_the_past() {
        assert_eq!(absolute_deadline_ticks(100, 23), 123);
        assert_eq!(absolute_deadline_ticks(u64::MAX - 2, 3), u64::MAX);
    }

    #[test]
    fn conversions_saturate_instead_of_wrapping() {
        assert_eq!(
            ticks_to_nanos_checked(2_400_000_000, 2_400_000_000),
            1_000_000_000
        );
        assert_eq!(
            nanos_to_ticks_checked(1_000_000_000, 2_400_000_000),
            2_400_000_000
        );
        assert_eq!(ticks_to_nanos_checked(u64::MAX, 1), u64::MAX);
        assert_eq!(nanos_to_ticks_checked(u64::MAX, u64::MAX), u64::MAX);
        assert_eq!(ticks_to_nanos_checked(10, 0), 0);
        assert_eq!(nanos_to_ticks_checked(10, 0), 0);
    }

    #[test]
    fn measured_frequency_uses_wide_intermediate() {
        assert_eq!(frequency_from_counts(u64::MAX, 1, 1), Some(u64::MAX));
        assert_eq!(frequency_from_counts(0, 1, 1), None);
        assert_eq!(frequency_from_counts(1, 0, 1), None);
    }

    #[test]
    fn hpet_window_conversion_is_overflow_safe() {
        assert_eq!(
            hpet_ticks_for_nanos(10_000_000, 10_000_000),
            Some(1_000_000)
        );
        assert_eq!(hpet_ticks_for_nanos(0, 10), None);
    }

    #[test]
    fn reference_counter_delta_handles_32_bit_wrap() {
        assert_eq!(counter_elapsed(u64::MAX - 2, 1, false), 4);
        assert_eq!(counter_elapsed(u64::MAX - 2, 1, true), 4);
    }

    #[cfg(feature = "irq")]
    #[test]
    fn lapic_deadline_conversion_is_bounded_to_counter_width() {
        assert_eq!(lapic_ticks_for_nanos(0, 100_000_000), 1);
        assert_eq!(
            lapic_ticks_for_nanos(1_000_000_000, 100_000_000),
            100_000_000
        );
        assert_eq!(lapic_ticks_for_nanos(u64::MAX, u64::MAX), u32::MAX);
    }
}
