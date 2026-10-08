//! MMIO adapter for the translated display-12/13 DP AUX transaction engine.
//!
//! This initial kernel binding admits only AUX A/B and an already-connected
//! external DP port. It polls because the current HPD IRQ path does not yet
//! dispatch AUX completion interrupts. PPS/eDP and DPCD reads are deliberately
//! separate call-site decisions; construction does not start a transaction.

use alloc::{string::String, vec::Vec};

use intel_display::{
    dp_aux::{AuxChannel, AuxError, AuxPlatform, AuxRegister, AuxState, DpAuxIo},
    power_map::PowerDomain,
};
use spin::MutexGuard;

use super::{
    POWER,
    gmbus::{MonotonicTimer, PollTimer},
    regs::{
        Register, RegisterWindow, Registers,
        aux::{
            DP_AUX_CH_CTL_A, DP_AUX_CH_CTL_B, DP_AUX_CH_DATA0_A, DP_AUX_CH_DATA0_B,
            DP_AUX_CH_DATA1_A, DP_AUX_CH_DATA1_B, DP_AUX_CH_DATA2_A, DP_AUX_CH_DATA2_B,
            DP_AUX_CH_DATA3_A, DP_AUX_CH_DATA3_B, DP_AUX_CH_DATA4_A, DP_AUX_CH_DATA4_B,
        },
    },
};

static AUX_A_LOCK: spin::Mutex<()> = spin::Mutex::new(());
static AUX_B_LOCK: spin::Mutex<()> = spin::Mutex::new(());

const POWER_DOMAIN_A: u16 = 0;
const POWER_DOMAIN_B: u16 = 1;

pub(crate) struct DpAuxKernel<'a> {
    registers: &'a RegisterWindow,
    timer: MonotonicTimer,
    connected: bool,
    channel: AuxChannel,
    port_lock: Option<MutexGuard<'static, ()>>,
    power_domain: PowerDomain,
    diagnostics: Vec<String>,
}

impl<'a> DpAuxKernel<'a> {
    pub(crate) fn new(
        registers: &'a RegisterWindow,
        channel: AuxChannel,
        connected: bool,
    ) -> Result<Self, AuxError> {
        let power_domain = match channel {
            AuxChannel::A => PowerDomain::AuxA,
            AuxChannel::B => PowerDomain::AuxB,
            _ => return Err(AuxError::Invalid),
        };
        Ok(Self {
            registers,
            timer: MonotonicTimer,
            connected,
            channel,
            port_lock: None,
            power_domain,
            diagnostics: Vec::new(),
        })
    }

    pub(crate) fn state(&self, encoder_name: &str) -> AuxState {
        AuxState {
            channel: self.channel,
            platform: aux_platform(),
            name: String::new(),
            last_busy_status: 0,
            pm_qos_active: false,
            edp: false,
            tbt_alt_mode: false,
            firmware_sync_length_quirk: false,
            aux_domain: if self.channel == AuxChannel::A {
                POWER_DOMAIN_A
            } else {
                POWER_DOMAIN_B
            },
            encoder_name: String::from(encoder_name),
        }
    }

    pub(crate) fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    fn register(register: AuxRegister) -> Result<Register, AuxError> {
        if register.generation != intel_display::dp_aux::AuxGeneration::Tigerlake || register.pch {
            return Err(AuxError::Invalid);
        }
        match (register.channel, register.data_index) {
            (AuxChannel::A, None) => Ok(DP_AUX_CH_CTL_A),
            (AuxChannel::B, None) => Ok(DP_AUX_CH_CTL_B),
            (AuxChannel::A, Some(0)) => Ok(DP_AUX_CH_DATA0_A),
            (AuxChannel::A, Some(1)) => Ok(DP_AUX_CH_DATA1_A),
            (AuxChannel::A, Some(2)) => Ok(DP_AUX_CH_DATA2_A),
            (AuxChannel::A, Some(3)) => Ok(DP_AUX_CH_DATA3_A),
            (AuxChannel::A, Some(4)) => Ok(DP_AUX_CH_DATA4_A),
            (AuxChannel::B, Some(0)) => Ok(DP_AUX_CH_DATA0_B),
            (AuxChannel::B, Some(1)) => Ok(DP_AUX_CH_DATA1_B),
            (AuxChannel::B, Some(2)) => Ok(DP_AUX_CH_DATA2_B),
            (AuxChannel::B, Some(3)) => Ok(DP_AUX_CH_DATA3_B),
            (AuxChannel::B, Some(4)) => Ok(DP_AUX_CH_DATA4_B),
            _ => Err(AuxError::Invalid),
        }
    }

    fn domain(id: u16) -> Result<PowerDomain, AuxError> {
        match id {
            POWER_DOMAIN_A => Ok(PowerDomain::AuxA),
            POWER_DOMAIN_B => Ok(PowerDomain::AuxB),
            _ => Err(AuxError::Invalid),
        }
    }

    fn power_get_domain(&mut self, domain: PowerDomain) -> Result<(), AuxError> {
        let mut power = POWER.lock();
        let state = power.as_mut().ok_or(AuxError::Power)?;
        state
            .get_domain(self.registers, domain)
            .map_err(|_| AuxError::Power)
    }

    fn power_put_domain(&mut self, domain: PowerDomain) {
        let mut power = POWER.lock();
        if let Some(state) = power.as_mut() {
            if state.put_domain(self.registers, domain).is_err() {
                self.diagnostics
                    .push(String::from("AUX power-domain put failed"));
            }
        } else {
            self.diagnostics
                .push(String::from("AUX power state disappeared"));
        }
    }
}

impl DpAuxIo for DpAuxKernel<'_> {
    fn lock_port(&mut self) -> Result<(), AuxError> {
        if self.port_lock.is_some() {
            return Err(AuxError::Busy);
        }
        self.port_lock = Some(match self.channel {
            AuxChannel::A => AUX_A_LOCK.lock(),
            AuxChannel::B => AUX_B_LOCK.lock(),
            _ => return Err(AuxError::Invalid),
        });
        Ok(())
    }

    fn unlock_port(&mut self) {
        self.port_lock.take();
    }

    fn connected(&mut self) -> bool {
        self.connected
    }

    fn power_get(&mut self, domain: u16) -> Result<(), AuxError> {
        self.power_get_domain(Self::domain(domain)?)
    }

    fn power_put_async(&mut self, domain: u16) {
        // The current display workqueue has no delayed power-put binding; use
        // the manager's balanced synchronous release rather than leak a ref.
        if let Ok(domain) = Self::domain(domain) {
            self.power_put_domain(domain);
        } else {
            self.diagnostics
                .push(String::from("invalid AUX domain on put"));
        }
    }

    fn pps_lock(&mut self) -> Result<bool, AuxError> {
        Ok(false)
    }

    fn pps_vdd_on(&mut self) -> bool {
        false
    }

    fn pps_check_power(&mut self) {}

    fn pps_vdd_off(&mut self) {}

    fn pps_unlock(&mut self) {}

    fn cpu_latency_qos(&mut self, _latency_us: Option<u32>) {
        // The current x86 platform layer has no PM-QoS interface. AUX waits
        // remain busy-poll bounded here, which keeps this caller out of sleep.
    }

    fn read(&mut self, register: AuxRegister) -> Result<u32, AuxError> {
        self.registers
            .read(Self::register(register)?)
            .ok_or(AuxError::Io)
    }

    fn write(&mut self, register: AuxRegister, value: u32) -> Result<(), AuxError> {
        if self.registers.write(Self::register(register)?, value) {
            Ok(())
        } else {
            Err(AuxError::Io)
        }
    }

    fn wait_send_done(&mut self, register: AuxRegister, timeout_ms: u32) -> Result<u32, AuxError> {
        let start = self.timer.now_micros();
        let timeout = u64::from(timeout_ms) * 1_000;
        loop {
            let status = self.read(register)?;
            if status & (1 << 31) == 0 {
                return Ok(status);
            }
            if self.timer.now_micros().saturating_sub(start) >= timeout {
                return Err(AuxError::Timeout);
            }
            self.timer.pause();
        }
    }

    fn sleep_ms(&mut self, milliseconds: u32) {
        let until = self
            .timer
            .now_micros()
            .saturating_add(u64::from(milliseconds) * 1_000);
        while self.timer.now_micros() < until {
            self.timer.pause();
        }
    }

    fn sleep_us_range(&mut self, minimum: u32, _maximum: u32) {
        let until = self.timer.now_micros().saturating_add(u64::from(minimum));
        while self.timer.now_micros() < until {
            self.timer.pause();
        }
    }

    fn trace_register(&mut self, _register: AuxRegister, _value: u32) {}

    fn diagnostic(&mut self, diagnostic: intel_display::dp_aux::AuxDiagnostic) {
        self.diagnostics.push(alloc::format!("{diagnostic:?}"));
    }

    fn probe_dpcd(&mut self, _enabled: bool) {}

    fn wake_aux_waiters(&mut self) {}

    fn register_aux(&mut self) -> Result<(), AuxError> {
        Ok(())
    }
}

pub(crate) const fn aux_platform() -> AuxPlatform {
    AuxPlatform {
        display_version: 13,
        has_pch_split: false,
        valleyview: false,
        cherryview: false,
        broadwell: false,
        haswell: false,
        pch_lpt_h: false,
        rawclk_khz: 24_000,
        cdclk_khz: 0,
    }
}

/// Read a bounded DPCD span through the source-shaped AUX message helper.
/// Native AUX reads carry at most 16 payload bytes per request.
pub(crate) fn read_dpcd(
    registers: &RegisterWindow,
    channel: AuxChannel,
    connected: bool,
    address: u32,
    length: usize,
) -> Result<Vec<u8>, AuxError> {
    use intel_display::dp_aux::{AUX_NATIVE_READ, intel_dp_aux_transfer};

    if length > 256 {
        return Err(AuxError::TooBig);
    }
    let mut io = DpAuxKernel::new(registers, channel, connected)?;
    let name = if channel == AuxChannel::A {
        "DDI A"
    } else {
        "DDI B"
    };
    let mut state = io.state(name);
    let mut data = alloc::vec![0; length];
    let mut offset = 0;
    while offset < length {
        let chunk = (length - offset).min(16);
        let mut reply_data = [0u8; 16];
        let reply = intel_dp_aux_transfer(
            &mut io,
            &mut state,
            AUX_NATIVE_READ,
            address.saturating_add(offset as u32),
            None,
            &mut reply_data[..chunk],
        );
        if reply.reply != 0 || reply.bytes != chunk as i32 {
            return Err(match reply.bytes {
                -7 => AuxError::TooBig,
                -16 => AuxError::Busy,
                -110 => AuxError::Timeout,
                n if n < 0 => AuxError::Io,
                _ => AuxError::Invalid,
            });
        }
        data[offset..offset + chunk].copy_from_slice(&reply_data[..chunk]);
        offset += chunk;
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use intel_display::dp_aux::intel_dp_aux_register;

    use super::*;

    #[test]
    fn maps_only_typed_adl_n_aux_a_b_registers() {
        let platform = aux_platform();
        let aux_a = intel_dp_aux_register(platform, AuxChannel::A, Some(4));
        let aux_b = intel_dp_aux_register(platform, AuxChannel::B, Some(4));
        assert_eq!(DpAuxKernel::register(aux_a).unwrap().offset(), 0x64024);
        assert_eq!(DpAuxKernel::register(aux_b).unwrap().offset(), 0x64124);
        let unsupported = intel_dp_aux_register(platform, AuxChannel::C, None);
        assert_eq!(DpAuxKernel::register(unsupported), Err(AuxError::Invalid));
    }
}
