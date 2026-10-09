//! TheKernel adapter for the source-translated i915 GMBUS state machine.

use core::sync::atomic::{AtomicBool, Ordering};

use intel_display::{
    intel_gmbus_full::{
        self as source, Access, GmbusBus, GmbusDisplay, GmbusError as SourceError, GmbusGpio,
        GmbusIo, GmbusPlatform, I2cMessage, PchType, PollResult,
    },
    power_map::PowerDomain,
};
use spin::MutexGuard;

use super::{
    POWER,
    gmbus::{Pin, PollTimer, Rate},
    regs::{
        GMBUS0, GMBUS1, GMBUS2, GMBUS3, GMBUS4, GMBUS5, GPIO_B, GPIO_C, GPIO_D, GPIO_J, GPIO_K,
        GPIO_L, GPIO_M, Register, Registers,
    },
};

const GMBUS_MMIO_BASE: u32 = 0x000c_0000;
const I2C_RISEFALL_US: u32 = 10;
const I2C_CLOCK_STRETCH_TIMEOUT_US: u32 = 2_200;
static I2C_BITBANG_LOCK: spin::Mutex<()> = spin::Mutex::new(());
static GMBUS_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Maps the C transaction engine's offset and I/O hooks onto typed registers.
pub(crate) struct KernelGmbusIo<'a, R: Registers, T: PollTimer> {
    registers: &'a R,
    timer: &'a T,
    display: GmbusDisplay,
    bus: GmbusBus,
    last_status: u32,
    io_failure: Option<super::gmbus::GmbusError>,
    idle_timeout: bool,
    was_in_use: bool,
    stale_two_byte_index: bool,
    power_held: bool,
    mutex_guard: Option<MutexGuard<'static, ()>>,
}

impl<'a, R: Registers, T: PollTimer> KernelGmbusIo<'a, R, T> {
    pub(crate) fn new(registers: &'a R, timer: &'a T, pin: Pin, rate: Rate) -> Self {
        let display = GmbusDisplay {
            mmio_base: GMBUS_MMIO_BASE,
            display_version: 13,
            pch_type: PchType::Icp,
            platform: GmbusPlatform::default(),
            has_pch_cnp: false,
            has_pch_spt: false,
            has_gmch: false,
            wa_16025573575: false,
        };
        let pin_index = pin.index();
        let gpio = match pin {
            Pin::DdiA => GmbusGpio::B,
            Pin::DdiB => GmbusGpio::C,
            Pin::DdiC => GmbusGpio::D,
            Pin::Tc1 => GmbusGpio::J,
            Pin::Tc2 => GmbusGpio::K,
            Pin::Tc3 => GmbusGpio::L,
            Pin::Tc4 => GmbusGpio::M,
        };
        let bus = GmbusBus {
            display,
            adapter_name: match pin {
                Pin::DdiA => "i915 gmbus dpa",
                Pin::DdiB => "i915 gmbus dpb",
                Pin::DdiC => "i915 gmbus dpc",
                Pin::Tc1 => "i915 gmbus tc1",
                Pin::Tc2 => "i915 gmbus tc2",
                Pin::Tc3 => "i915 gmbus tc3",
                Pin::Tc4 => "i915 gmbus tc4",
            },
            force_bit: 0,
            reg0: pin_index | rate.field() << 8,
            gpio_reg: source::gpio_register(display, gpio),
        };
        Self {
            registers,
            timer,
            display,
            bus,
            last_status: 0,
            io_failure: None,
            idle_timeout: false,
            was_in_use: false,
            stale_two_byte_index: false,
            power_held: false,
            mutex_guard: None,
        }
    }

    pub(crate) fn bus(&self) -> GmbusBus {
        self.bus
    }

    pub(crate) fn bus_mut(&mut self) -> &mut GmbusBus {
        &mut self.bus
    }

    pub(crate) fn last_status(&self) -> u32 {
        self.last_status
    }

    fn register(&self, address: u32) -> Result<Register, SourceError> {
        let Some(offset) = address.checked_sub(self.display.mmio_base) else {
            return Err(SourceError::Io);
        };
        match offset {
            0x5100 => Ok(GMBUS0),
            0x5104 => Ok(GMBUS1),
            0x5108 => Ok(GMBUS2),
            0x510c => Ok(GMBUS3),
            0x5110 => Ok(GMBUS4),
            0x5120 => Ok(GMBUS5),
            0x5014 => Ok(GPIO_B),
            0x5018 => Ok(GPIO_C),
            0x501c => Ok(GPIO_D),
            0x5034 => Ok(GPIO_J),
            0x5038 => Ok(GPIO_K),
            0x503c => Ok(GPIO_L),
            0x5040 => Ok(GPIO_M),
            _ => Err(SourceError::Io),
        }
    }

    fn read_register(&mut self, address: u32) -> Result<u32, SourceError> {
        let register = self.register(address)?;
        let value = self.registers.read(register).ok_or_else(|| {
            self.io_failure = Some(super::gmbus::GmbusError::WindowTooSmall {
                register: register.name(),
            });
            SourceError::Io
        })?;
        if register == GMBUS2 {
            self.last_status = value;
        }
        Ok(value)
    }

    fn wait_clock_high(&mut self) -> Result<(), SourceError> {
        let start = self.timer.now_micros();
        loop {
            let bus = self.bus;
            if source::get_clock(self, &bus)? {
                return Ok(());
            }
            if self.timer.now_micros().saturating_sub(start)
                >= u64::from(I2C_CLOCK_STRETCH_TIMEOUT_US)
            {
                return Err(SourceError::Timeout);
            }
            self.timer.pause();
        }
    }

    fn start_condition(&mut self) -> Result<(), SourceError> {
        let bus = self.bus;
        source::set_data(self, &bus, true)?;
        source::set_clock(self, &bus, true)?;
        self.delay_us(I2C_RISEFALL_US);
        source::set_data(self, &bus, false)?;
        self.delay_us(I2C_RISEFALL_US);
        source::set_clock(self, &bus, false)
    }

    fn stop_condition(&mut self) -> Result<(), SourceError> {
        let bus = self.bus;
        source::set_data(self, &bus, false)?;
        source::set_clock(self, &bus, true)?;
        self.wait_clock_high()?;
        self.delay_us(I2C_RISEFALL_US);
        source::set_data(self, &bus, true)?;
        self.delay_us(I2C_RISEFALL_US);
        Ok(())
    }

    fn write_byte(&mut self, value: u8) -> Result<(), SourceError> {
        let bus = self.bus;
        for bit in (0..8).rev() {
            source::set_clock(self, &bus, false)?;
            source::set_data(self, &bus, value & (1 << bit) != 0)?;
            self.delay_us(I2C_RISEFALL_US);
            source::set_clock(self, &bus, true)?;
            self.wait_clock_high()?;
            self.delay_us(I2C_RISEFALL_US);
        }
        source::set_clock(self, &bus, false)?;
        source::set_data(self, &bus, true)?;
        self.delay_us(I2C_RISEFALL_US);
        source::set_clock(self, &bus, true)?;
        self.wait_clock_high()?;
        let acknowledged = !source::get_data(self, &bus)?;
        self.delay_us(I2C_RISEFALL_US);
        source::set_clock(self, &bus, false)?;
        if acknowledged {
            Ok(())
        } else {
            Err(SourceError::NoDevice)
        }
    }

    fn read_byte(&mut self, acknowledge: bool) -> Result<u8, SourceError> {
        let mut value = 0;
        let bus = self.bus;
        source::set_data(self, &bus, true)?;
        for _ in 0..8 {
            source::set_clock(self, &bus, true)?;
            self.wait_clock_high()?;
            self.delay_us(I2C_RISEFALL_US);
            value = (value << 1) | u8::from(source::get_data(self, &bus)?);
            source::set_clock(self, &bus, false)?;
            self.delay_us(I2C_RISEFALL_US);
        }
        source::set_data(self, &bus, !acknowledge)?;
        source::set_clock(self, &bus, true)?;
        self.wait_clock_high()?;
        self.delay_us(I2C_RISEFALL_US);
        source::set_clock(self, &bus, false)?;
        source::set_data(self, &bus, true)?;
        Ok(value)
    }
}

impl<R: Registers, T: PollTimer> GmbusIo for KernelGmbusIo<'_, R, T> {
    fn read32(&mut self, register: u32, _access: Access) -> Result<u32, SourceError> {
        self.read_register(register)
    }

    fn write32(&mut self, register: u32, value: u32, _access: Access) -> Result<(), SourceError> {
        let register = self.register(register)?;
        if self.registers.write(register, value) {
            Ok(())
        } else {
            self.io_failure = Some(super::gmbus::GmbusError::RegisterRefused {
                register: register.name(),
            });
            Err(SourceError::Io)
        }
    }

    fn posting_read(&mut self, register: u32) -> Result<u32, SourceError> {
        self.read_register(register)
    }

    fn rmw32(&mut self, register: u32, clear: u32, set: u32) -> Result<(), SourceError> {
        let register = self.register(register)?;
        let old = self.registers.read(register).ok_or(SourceError::Io)?;
        if self.registers.write(register, (old & !clear) | set) {
            Ok(())
        } else {
            Err(SourceError::Io)
        }
    }

    fn delay_us(&mut self, microseconds: u32) {
        let until = self
            .timer
            .now_micros()
            .saturating_add(u64::from(microseconds));
        while self.timer.now_micros() < until {
            self.timer.pause();
        }
    }

    fn poll_timeout_us_atomic(
        &mut self,
        register: u32,
        mask: u32,
        timeout_us: u32,
    ) -> Result<PollResult, SourceError> {
        self.poll_timeout_us(register, mask, 0, timeout_us)
    }

    fn poll_timeout_us(
        &mut self,
        register: u32,
        mask: u32,
        interval_us: u32,
        timeout_us: u32,
    ) -> Result<PollResult, SourceError> {
        let start = self.timer.now_micros();
        let deadline = start.saturating_add(u64::from(timeout_us));
        loop {
            let status = self.read_register(register)?;
            if status & mask != 0 {
                return Ok(PollResult {
                    status,
                    matched: true,
                });
            }
            if self.timer.now_micros() >= deadline {
                return Ok(PollResult {
                    status,
                    matched: false,
                });
            }
            if interval_us == 0 {
                self.timer.pause();
            } else {
                self.delay_us(interval_us);
            }
        }
    }

    fn wait_register_mask_ms(
        &mut self,
        register: u32,
        mask: u32,
        expected: u32,
        timeout_ms: u32,
    ) -> Result<(), SourceError> {
        let deadline = self
            .timer
            .now_micros()
            .saturating_add(u64::from(timeout_ms) * 1_000);
        loop {
            if self.read_register(register)? & mask == expected {
                return Ok(());
            }
            if self.timer.now_micros() >= deadline {
                self.idle_timeout = true;
                return Err(SourceError::Timeout);
            }
            self.timer.pause();
        }
    }

    fn add_waiter(&mut self) {}
    fn remove_waiter(&mut self) {}
    fn wake_waiters(&mut self) {}
    fn parent_irq_enabled(&self) -> bool {
        false
    }

    fn display_power_get(&mut self) -> Result<(), SourceError> {
        #[cfg(target_os = "none")]
        {
            let mut power = POWER.lock();
            let state = power.as_mut().ok_or(SourceError::Power)?;
            state
                .get_domain(self.registers, PowerDomain::Gmbus)
                .map_err(|_| SourceError::Power)?;
        }
        self.power_held = true;
        if let Ok(status) = self.read32(GMBUS_MMIO_BASE + 0x5108, Access::Firmware) {
            self.was_in_use = status & (1 << 15) != 0;
        }
        let index_state = (|| {
            let index = self.read32(GMBUS_MMIO_BASE + 0x5120, Access::Firmware)?;
            self.stale_two_byte_index = index & (1 << 31) != 0;
            if self.stale_two_byte_index {
                self.write32(GMBUS_MMIO_BASE + 0x5120, 0, Access::Firmware)?;
            }
            Ok::<(), SourceError>(())
        })();
        if let Err(error) = index_state {
            self.display_power_put();
            return Err(error);
        }
        if !GMBUS_INITIALIZED.load(Ordering::Acquire) {
            let display = self.display;
            if let Err(error) = source::intel_gmbus_reset(self, display) {
                self.display_power_put();
                return Err(error);
            }
            GMBUS_INITIALIZED.store(true, Ordering::Release);
        }
        Ok(())
    }

    fn display_power_put(&mut self) {
        if !self.power_held {
            return;
        }
        #[cfg(target_os = "none")]
        {
            let mut power = POWER.lock();
            if let Some(state) = power.as_mut() {
                let _ = state.put_domain(self.registers, PowerDomain::Gmbus);
            }
        }
        self.power_held = false;
    }

    fn gmbus_mutex_lock(&mut self) {
        self.mutex_guard = Some(I2C_BITBANG_LOCK.lock());
    }

    fn gmbus_mutex_unlock(&mut self) {
        self.mutex_guard.take();
    }

    fn bitbang_master_xfer(
        &mut self,
        messages: &mut [I2cMessage<'_>],
    ) -> Result<usize, SourceError> {
        let bus = self.bus;
        source::intel_gpio_pre_xfer(self, &bus)?;
        let result = (|| {
            for message in messages.iter_mut() {
                if message.len > message.buffer.len() {
                    return Err(SourceError::Invalid);
                }
                self.start_condition()?;
                let address = ((message.address as u8) << 1) | u8::from(message.is_read());
                self.write_byte(address)?;
                if message.is_read() {
                    for index in 0..message.len {
                        message.buffer[index] = self.read_byte(index + 1 < message.len)?;
                    }
                } else {
                    for index in 0..message.len {
                        self.write_byte(message.buffer[index])?;
                    }
                }
            }
            self.stop_condition()?;
            Ok(messages.len())
        })();
        if result.is_err() {
            let _ = self.stop_condition();
        }
        let _ = source::intel_gpio_post_xfer(self, &bus);
        result
    }

    fn diagnostic(&mut self, diagnostic: source::GmbusDiagnostic) {
        axlog::debug!("intel-gmbus: {diagnostic:?}");
    }
}

pub(crate) struct GMBusTransfer {
    pub(crate) result: Result<usize, SourceError>,
    pub(crate) last_status: u32,
    pub(crate) io_failure: Option<super::gmbus::GmbusError>,
    pub(crate) idle_timeout: bool,
    pub(crate) elapsed_micros: u64,
    pub(crate) force_bit: u32,
    pub(crate) was_in_use: bool,
    pub(crate) stale_two_byte_index: bool,
}

/// Run the source GMBUS indexed write + data read used for one EDID block.
pub(crate) fn read_edid_block<R: Registers, T: PollTimer>(
    registers: &R,
    timer: &T,
    pin: Pin,
    rate: Rate,
    address: u8,
    offset: u8,
    output: &mut [u8],
    force_bit: u32,
) -> GMBusTransfer {
    let start = timer.now_micros();
    let mut io = KernelGmbusIo::new(registers, timer, pin, rate);
    let mut bus = io.bus();
    bus.force_bit = force_bit;
    let mut register_index = [offset];
    let mut messages = [
        I2cMessage {
            address: u16::from(address),
            flags: 0,
            len: 1,
            buffer: &mut register_index,
        },
        I2cMessage {
            address: u16::from(address),
            flags: 1,
            len: output.len(),
            buffer: output,
        },
    ];
    io.gmbus_mutex_lock();
    let result = source::gmbus_xfer(&mut io, &mut bus, &mut messages);
    io.gmbus_mutex_unlock();
    GMBusTransfer {
        result,
        last_status: io.last_status(),
        io_failure: io.io_failure,
        idle_timeout: io.idle_timeout,
        elapsed_micros: timer.now_micros().saturating_sub(start),
        force_bit: bus.force_bit,
        was_in_use: io.was_in_use,
        stale_two_byte_index: io.stale_two_byte_index,
    }
}
