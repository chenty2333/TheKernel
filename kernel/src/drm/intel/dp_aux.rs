//! MMIO adapter for the translated display-12/13 DP AUX transaction engine.
//!
//! This kernel binding admits AUX A/B plus the source-mapped TC1/TC2 Type-C AUX
//! channels (MMIO channels D/E) and an already-connected DP port. It polls
//! because the current HPD IRQ path does not yet dispatch AUX completion
//! interrupts. PPS/eDP and DPCD reads are deliberately separate call-site
//! decisions; construction does not start a transaction.

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
        Register, Registers,
        aux::{
            DP_AUX_CH_CTL_A, DP_AUX_CH_CTL_B, DP_AUX_CH_CTL_D, DP_AUX_CH_CTL_E, DP_AUX_CH_DATA0_A,
            DP_AUX_CH_DATA0_B, DP_AUX_CH_DATA0_D, DP_AUX_CH_DATA0_E, DP_AUX_CH_DATA1_A,
            DP_AUX_CH_DATA1_B, DP_AUX_CH_DATA1_D, DP_AUX_CH_DATA1_E, DP_AUX_CH_DATA2_A,
            DP_AUX_CH_DATA2_B, DP_AUX_CH_DATA2_D, DP_AUX_CH_DATA2_E, DP_AUX_CH_DATA3_A,
            DP_AUX_CH_DATA3_B, DP_AUX_CH_DATA3_D, DP_AUX_CH_DATA3_E, DP_AUX_CH_DATA4_A,
            DP_AUX_CH_DATA4_B, DP_AUX_CH_DATA4_D, DP_AUX_CH_DATA4_E,
        },
    },
};

static AUX_A_LOCK: spin::Mutex<()> = spin::Mutex::new(());
static AUX_B_LOCK: spin::Mutex<()> = spin::Mutex::new(());
static AUX_USBC1_LOCK: spin::Mutex<()> = spin::Mutex::new(());
static AUX_USBC2_LOCK: spin::Mutex<()> = spin::Mutex::new(());

const POWER_DOMAIN_A: u16 = 0;
const POWER_DOMAIN_B: u16 = 1;
// Private transport IDs passed through translated `AuxState`; these are not
// AUX-channel ordinals. Keep each AUX well independently reference-counted.
const POWER_DOMAIN_USBC1: u16 = 2;
const POWER_DOMAIN_USBC2: u16 = 3;

pub(crate) struct DpAuxKernel<'a, R: Registers> {
    registers: &'a R,
    timer: MonotonicTimer,
    connected: bool,
    channel: AuxChannel,
    port_lock: Option<MutexGuard<'static, ()>>,
    power_domain: PowerDomain,
    parent_power_domain: Option<PowerDomain>,
    diagnostics: Vec<String>,
}

impl<'a, R: Registers> DpAuxKernel<'a, R> {
    pub(crate) fn new(
        registers: &'a R,
        channel: AuxChannel,
        connected: bool,
    ) -> Result<Self, AuxError> {
        let power_domain = match channel {
            AuxChannel::A => PowerDomain::AuxA,
            AuxChannel::B => PowerDomain::AuxB,
            AuxChannel::UsbC1 => PowerDomain::AuxUsbc1,
            AuxChannel::UsbC2 => PowerDomain::AuxUsbc2,
            _ => return Err(AuxError::Invalid),
        };
        let parent_power_domain = match channel {
            AuxChannel::UsbC1 => Some(PowerDomain::PortDdiLanesTc1),
            AuxChannel::UsbC2 => Some(PowerDomain::PortDdiLanesTc2),
            _ => None,
        };
        Ok(Self {
            registers,
            timer: MonotonicTimer,
            connected,
            channel,
            port_lock: None,
            power_domain,
            parent_power_domain,
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
            aux_domain: match self.channel {
                AuxChannel::A => POWER_DOMAIN_A,
                AuxChannel::B => POWER_DOMAIN_B,
                AuxChannel::UsbC1 => POWER_DOMAIN_USBC1,
                AuxChannel::UsbC2 => POWER_DOMAIN_USBC2,
                // `new()` is the only constructor and rejects every other
                // channel before a DpAuxKernel can be created.
                _ => unreachable!("DpAuxKernel only admits typed AUX channels"),
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
            // On TGL/ADL the Rust UsbC1/UsbC2 enum values are distinct from
            // AUX_CH_D/AUX_CH_E. Source `_PICK_EVEN` maps these aliases to
            // the existing MMIO D/E register windows; do not use ordinals.
            (AuxChannel::UsbC1, None) => Ok(DP_AUX_CH_CTL_D),
            (AuxChannel::UsbC1, Some(0)) => Ok(DP_AUX_CH_DATA0_D),
            (AuxChannel::UsbC1, Some(1)) => Ok(DP_AUX_CH_DATA1_D),
            (AuxChannel::UsbC1, Some(2)) => Ok(DP_AUX_CH_DATA2_D),
            (AuxChannel::UsbC1, Some(3)) => Ok(DP_AUX_CH_DATA3_D),
            (AuxChannel::UsbC1, Some(4)) => Ok(DP_AUX_CH_DATA4_D),
            (AuxChannel::UsbC2, None) => Ok(DP_AUX_CH_CTL_E),
            (AuxChannel::UsbC2, Some(0)) => Ok(DP_AUX_CH_DATA0_E),
            (AuxChannel::UsbC2, Some(1)) => Ok(DP_AUX_CH_DATA1_E),
            (AuxChannel::UsbC2, Some(2)) => Ok(DP_AUX_CH_DATA2_E),
            (AuxChannel::UsbC2, Some(3)) => Ok(DP_AUX_CH_DATA3_E),
            (AuxChannel::UsbC2, Some(4)) => Ok(DP_AUX_CH_DATA4_E),
            _ => Err(AuxError::Invalid),
        }
    }

    fn domain(id: u16) -> Result<PowerDomain, AuxError> {
        match id {
            POWER_DOMAIN_A => Ok(PowerDomain::AuxA),
            POWER_DOMAIN_B => Ok(PowerDomain::AuxB),
            POWER_DOMAIN_USBC1 => Ok(PowerDomain::AuxUsbc1),
            POWER_DOMAIN_USBC2 => Ok(PowerDomain::AuxUsbc2),
            _ => Err(AuxError::Invalid),
        }
    }

    fn power_get_domain(&mut self, domain: PowerDomain) -> Result<(), AuxError> {
        if domain != self.power_domain {
            return Err(AuxError::Power);
        }
        let mut power = POWER.lock();
        let state = power.as_mut().ok_or(AuxError::Power)?;
        if let Some(parent) = self.parent_power_domain {
            state
                .get_domain(self.registers, parent)
                .map_err(|_| AuxError::Power)?;
        }
        if state.get_domain(self.registers, domain).is_err() {
            if let Some(parent) = self.parent_power_domain {
                let _ = state.put_domain(self.registers, parent);
            }
            return Err(AuxError::Power);
        }
        Ok(())
    }

    fn power_put_domain(&mut self, domain: PowerDomain) {
        if domain != self.power_domain {
            self.diagnostics
                .push(String::from("invalid AUX domain on put"));
            return;
        }
        let mut power = POWER.lock();
        if let Some(state) = power.as_mut() {
            if state.put_domain(self.registers, domain).is_err() {
                self.diagnostics
                    .push(String::from("AUX power-domain put failed"));
                // Keep the parent lane reference if the AUX reference could
                // not be proven released.
                return;
            }
            if let Some(parent) = self.parent_power_domain
                && state.put_domain(self.registers, parent).is_err()
            {
                self.diagnostics
                    .push(String::from("AUX parent lane-domain put failed"));
            }
        } else {
            self.diagnostics
                .push(String::from("AUX power state disappeared"));
        }
    }

    fn transfer(
        &mut self,
        request: u8,
        address: u32,
        payload: Option<&[u8]>,
        receive: &mut [u8],
    ) -> intel_display::dp_aux::AuxReply {
        let encoder_name = match self.channel {
            AuxChannel::A => "DDI A",
            AuxChannel::B => "DDI B",
            AuxChannel::UsbC1 => "DDI TC1",
            AuxChannel::UsbC2 => "DDI TC2",
            _ => unreachable!("DpAuxKernel only admits typed AUX channels"),
        };
        let mut state = self.state(encoder_name);
        intel_display::dp_aux::intel_dp_aux_transfer(
            self, &mut state, request, address, payload, receive,
        )
    }
}

impl<R: Registers> DpAuxIo for DpAuxKernel<'_, R> {
    fn lock_port(&mut self) -> Result<(), AuxError> {
        if self.port_lock.is_some() {
            return Err(AuxError::Busy);
        }
        self.port_lock = Some(match self.channel {
            AuxChannel::A => AUX_A_LOCK.lock(),
            AuxChannel::B => AUX_B_LOCK.lock(),
            AuxChannel::UsbC1 => AUX_USBC1_LOCK.lock(),
            AuxChannel::UsbC2 => AUX_USBC2_LOCK.lock(),
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

impl<R: Registers> intel_display::intel_dp_full::DpAuxIo for DpAuxKernel<'_, R> {
    fn read(
        &mut self,
        address: u32,
        bytes: &mut [u8],
    ) -> Result<usize, intel_display::intel_dp_full::DpError> {
        use intel_display::{dp_aux::AUX_NATIVE_READ, intel_dp_full::DpError};
        let mut offset = 0;
        while offset < bytes.len() {
            let length = (bytes.len() - offset).min(16);
            let mut first_error = None;
            let mut transferred = false;
            for _ in 0..32 {
                let reply = self.transfer(
                    AUX_NATIVE_READ,
                    address.saturating_add(offset as u32),
                    None,
                    &mut bytes[offset..offset + length],
                );
                let error = if reply.bytes < 0 {
                    reply.bytes
                } else if reply.reply & 0x03 != 0 {
                    -5
                } else if reply.bytes != length as i32 {
                    -71
                } else {
                    0
                };
                if error == 0 {
                    transferred = true;
                    break;
                }
                first_error.get_or_insert(error);
                if error != -110 {
                    self.sleep_us_range(500, 600);
                }
            }
            if !transferred {
                return Err(match first_error.unwrap_or(-5) {
                    -7 | -22 => DpError::Invalid,
                    _ => DpError::Io,
                });
            }
            offset += length;
        }
        Ok(offset)
    }

    fn write(
        &mut self,
        address: u32,
        bytes: &[u8],
    ) -> Result<usize, intel_display::intel_dp_full::DpError> {
        use intel_display::{dp_aux::AUX_NATIVE_WRITE, intel_dp_full::DpError};
        let mut offset = 0;
        while offset < bytes.len() {
            let length = (bytes.len() - offset).min(16);
            let mut first_error = None;
            let mut transferred = false;
            for _ in 0..32 {
                let mut reply_data = [0u8; 2];
                let reply = self.transfer(
                    AUX_NATIVE_WRITE,
                    address.saturating_add(offset as u32),
                    Some(&bytes[offset..offset + length]),
                    &mut reply_data,
                );
                let error = if reply.bytes < 0 {
                    reply.bytes
                } else if reply.reply & 0x03 != 0 {
                    -5
                } else if reply.bytes != length as i32 {
                    -71
                } else {
                    0
                };
                if error == 0 {
                    transferred = true;
                    break;
                }
                first_error.get_or_insert(error);
                if error != -110 {
                    self.sleep_us_range(500, 600);
                }
            }
            if !transferred {
                return Err(match first_error.unwrap_or(-5) {
                    -7 | -22 => DpError::Invalid,
                    _ => DpError::Io,
                });
            }
            offset += length;
        }
        Ok(offset)
    }

    fn delay_ms(&mut self, milliseconds: u32) {
        self.sleep_ms(milliseconds);
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
    registers: &impl Registers,
    channel: AuxChannel,
    connected: bool,
    address: u32,
    length: usize,
) -> Result<Vec<u8>, AuxError> {
    use intel_display::intel_dp_full::DpAuxIo as SourceDpAuxIo;

    if length > 256 {
        return Err(AuxError::TooBig);
    }
    let mut io = DpAuxKernel::new(registers, channel, connected)?;
    let mut data = alloc::vec![0; length];
    SourceDpAuxIo::read(&mut io, address, &mut data).map_err(|_| AuxError::Io)?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use intel_display::dp_aux::{AuxRegister, intel_dp_aux_register};

    use super::{super::regs::RegisterWindow, *};

    struct NoRegisters;
    impl Registers for NoRegisters {
        fn read(&self, _register: Register) -> Option<u32> {
            None
        }

        fn read64(&self, _register: Register) -> Option<u64> {
            None
        }

        fn write(&self, _register: Register, _value: u32) -> bool {
            false
        }
    }

    #[test]
    fn maps_only_typed_adl_n_aux_a_b_registers() {
        let platform = aux_platform();
        let aux_a = intel_dp_aux_register(platform, AuxChannel::A, Some(4));
        let aux_b = intel_dp_aux_register(platform, AuxChannel::B, Some(4));
        assert_eq!(
            DpAuxKernel::<RegisterWindow>::register(aux_a)
                .unwrap()
                .offset(),
            0x64024
        );
        assert_eq!(
            DpAuxKernel::<RegisterWindow>::register(aux_b)
                .unwrap()
                .offset(),
            0x64124
        );
        let unsupported = intel_dp_aux_register(platform, AuxChannel::C, None);
        assert_eq!(
            DpAuxKernel::<RegisterWindow>::register(unsupported),
            Err(AuxError::Invalid)
        );
    }

    #[test]
    fn maps_source_usbc1_usbc2_to_typed_aux_d_e_windows() {
        let platform = aux_platform();
        for (channel, base) in [(AuxChannel::UsbC1, 0x64310), (AuxChannel::UsbC2, 0x64410)] {
            assert_eq!(
                DpAuxKernel::<RegisterWindow>::register(intel_dp_aux_register(
                    platform, channel, None,
                ))
                .unwrap()
                .offset(),
                base
            );
            for index in 0..5u8 {
                assert_eq!(
                    DpAuxKernel::<RegisterWindow>::register(intel_dp_aux_register(
                        platform,
                        channel,
                        Some(index),
                    ))
                    .unwrap()
                    .offset(),
                    base + 4 + u32::from(index) * 4
                );
            }
        }
    }

    #[test]
    fn type_c_aux_references_hold_the_matching_ddi_lane_well() {
        let registers = NoRegisters;
        let tc1 = DpAuxKernel::new(&registers, AuxChannel::UsbC1, true).unwrap();
        assert_eq!(tc1.power_domain, PowerDomain::AuxUsbc1);
        assert_eq!(tc1.parent_power_domain, Some(PowerDomain::PortDdiLanesTc1));
        let tc2 = DpAuxKernel::new(&registers, AuxChannel::UsbC2, true).unwrap();
        assert_eq!(tc2.power_domain, PowerDomain::AuxUsbc2);
        assert_eq!(tc2.parent_power_domain, Some(PowerDomain::PortDdiLanesTc2));
        assert!(DpAuxKernel::new(&registers, AuxChannel::UsbC3, true).is_err());
    }

    #[test]
    fn type_c_aux_domain_ids_are_distinct_and_other_channels_stay_refused() {
        assert_eq!(
            DpAuxKernel::<RegisterWindow>::domain(POWER_DOMAIN_USBC1),
            Ok(PowerDomain::AuxUsbc1)
        );
        assert_eq!(
            DpAuxKernel::<RegisterWindow>::domain(POWER_DOMAIN_USBC2),
            Ok(PowerDomain::AuxUsbc2)
        );
        for channel in [
            AuxChannel::C,
            AuxChannel::D,
            AuxChannel::E,
            AuxChannel::F,
            AuxChannel::UsbC3,
            AuxChannel::UsbC4,
            AuxChannel::UsbC5,
            AuxChannel::UsbC6,
        ] {
            let register = AuxRegister {
                generation: intel_display::dp_aux::AuxGeneration::Tigerlake,
                channel,
                data_index: None,
                pch: false,
            };
            assert_eq!(
                DpAuxKernel::<RegisterWindow>::register(register),
                Err(AuxError::Invalid),
                "unexpectedly mapped {channel:?}"
            );
        }
        assert_eq!(
            DpAuxKernel::<RegisterWindow>::domain(4),
            Err(AuxError::Invalid)
        );
    }
}
