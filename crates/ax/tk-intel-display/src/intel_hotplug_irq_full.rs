// SPDX-License-Identifier: MIT
// Copyright © 2023 Intel Corporation
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_hotplug_irq.c.
//! Low-level Intel display hotplug IRQ translation.
//!
//! Register access, interrupt-controller integration, lock primitives, BIOS
//! metadata, and the DRM IRQ dispatcher are explicit `HotplugIrqIo` hooks.
//! The translation retains the source pin maps, generation selection, W1C
//! acknowledgement retry bound, long-pulse tests, and interrupt sequencing.

extern crate alloc;

pub type HpdPin = u8;
pub const HPD_NONE: HpdPin = 0;
pub const HPD_CRT: HpdPin = 1;
pub const HPD_SDVO_B: HpdPin = 2;
pub const HPD_SDVO_C: HpdPin = 3;
pub const HPD_PORT_A: HpdPin = 4;
pub const HPD_PORT_B: HpdPin = 5;
pub const HPD_PORT_C: HpdPin = 6;
pub const HPD_PORT_D: HpdPin = 7;
pub const HPD_PORT_E: HpdPin = 8;
pub const HPD_PORT_TC1: HpdPin = 9;
pub const HPD_PORT_TC2: HpdPin = 10;
pub const HPD_PORT_TC3: HpdPin = 11;
pub const HPD_PORT_TC4: HpdPin = 12;
pub const HPD_PORT_TC5: HpdPin = 13;
pub const HPD_PORT_TC6: HpdPin = 14;
pub const HPD_NUM_PINS: usize = 16;

const fn bit(n: u32) -> u32 {
    1u32 << n
}
const fn pin_bit(pin: HpdPin) -> u32 {
    if pin < 32 { bit(pin as u32) } else { 0 }
}
const fn ddi_index(pin: HpdPin) -> u32 {
    pin.saturating_sub(HPD_PORT_A) as u32
}
const fn tc_index(pin: HpdPin) -> u32 {
    pin.saturating_sub(HPD_PORT_TC1) as u32
}
const fn gen8_de_port_hotplug(pin: HpdPin) -> u32 {
    bit(3 + ddi_index(pin))
}
const fn gen11_tc_hotplug(pin: HpdPin) -> u32 {
    bit(16 + tc_index(pin))
}
const fn gen11_tbt_hotplug(pin: HpdPin) -> u32 {
    bit(tc_index(pin))
}
const fn xelpdp_dp_alt_hotplug(pin: HpdPin) -> u32 {
    bit(16 + tc_index(pin))
}
const fn xelpdp_tbt_hotplug(pin: HpdPin) -> u32 {
    bit(tc_index(pin))
}
const fn sde_ddi_hotplug_icp(pin: HpdPin) -> u32 {
    bit(16 + ddi_index(pin))
}
const fn sde_tc_hotplug_icp(pin: HpdPin) -> u32 {
    bit(24 + tc_index(pin))
}
const fn sde_tc_hotplug_dg2(pin: HpdPin) -> u32 {
    bit(25 + tc_index(pin))
}

pub const PORT_HOTPLUG_INT_EN_B: u32 = bit(29);
pub const PORT_HOTPLUG_INT_EN_C: u32 = bit(28);
pub const PORT_HOTPLUG_INT_EN_D: u32 = bit(27);
pub const SDVOB_HOTPLUG_INT_EN: u32 = bit(26);
pub const SDVOC_HOTPLUG_INT_EN: u32 = bit(25);
pub const CRT_HOTPLUG_INT_EN: u32 = bit(9);
pub const HOTPLUG_INT_EN_MASK: u32 = PORT_HOTPLUG_INT_EN_B
    | PORT_HOTPLUG_INT_EN_C
    | PORT_HOTPLUG_INT_EN_D
    | SDVOC_HOTPLUG_INT_EN
    | SDVOB_HOTPLUG_INT_EN
    | CRT_HOTPLUG_INT_EN;
pub const CRT_HOTPLUG_ACTIVATION_PERIOD_64: u32 = bit(8);
pub const CRT_HOTPLUG_VOLTAGE_COMPARE_50: u32 = bit(5);
pub const CRT_HOTPLUG_VOLTAGE_COMPARE_MASK: u32 = 3 << 5;

pub const PORTB_HOTPLUG_INT_STATUS: u32 = 3 << 17;
pub const PORTC_HOTPLUG_INT_STATUS: u32 = 3 << 19;
pub const PORTD_HOTPLUG_INT_STATUS: u32 = 3 << 21;
pub const CRT_HOTPLUG_INT_STATUS: u32 = bit(11);
pub const DP_AUX_CHANNEL_MASK_INT_STATUS_G4X: u32 = 7 << 4;
pub const HOTPLUG_INT_STATUS_G4X: u32 = CRT_HOTPLUG_INT_STATUS
    | (1 << 2)
    | (1 << 3)
    | PORTB_HOTPLUG_INT_STATUS
    | PORTC_HOTPLUG_INT_STATUS
    | PORTD_HOTPLUG_INT_STATUS;
pub const HOTPLUG_INT_STATUS_I915: u32 = CRT_HOTPLUG_INT_STATUS
    | (1 << 6)
    | (1 << 7)
    | PORTB_HOTPLUG_INT_STATUS
    | PORTC_HOTPLUG_INT_STATUS
    | PORTD_HOTPLUG_INT_STATUS;

pub const PORTA_HOTPLUG_ENABLE: u32 = bit(28);
pub const BXT_DDIA_HPD_INVERT: u32 = bit(27);
pub const PORTA_HOTPLUG_LONG_DETECT: u32 = 2 << 24;
pub const PORTD_HOTPLUG_ENABLE: u32 = bit(20);
pub const PORTD_PULSE_DURATION_MASK: u32 = 3 << 18;
pub const PORTD_PULSE_DURATION_2MS: u32 = 0 << 18;
pub const PORTD_HOTPLUG_LONG_DETECT: u32 = 2 << 16;
pub const PORTC_HOTPLUG_ENABLE: u32 = bit(12);
pub const BXT_DDIC_HPD_INVERT: u32 = bit(11);
pub const PORTC_PULSE_DURATION_MASK: u32 = 3 << 10;
pub const PORTC_PULSE_DURATION_2MS: u32 = 0 << 10;
pub const PORTC_HOTPLUG_LONG_DETECT: u32 = 2 << 8;
pub const PORTB_HOTPLUG_ENABLE: u32 = bit(4);
pub const BXT_DDIB_HPD_INVERT: u32 = bit(3);
pub const PORTB_PULSE_DURATION_MASK: u32 = 3 << 2;
pub const PORTB_PULSE_DURATION_2MS: u32 = 0 << 2;
pub const PORTB_HOTPLUG_LONG_DETECT: u32 = 2;
pub const PORTE_HOTPLUG_ENABLE: u32 = bit(4);
pub const PORTE_HOTPLUG_LONG_DETECT: u32 = 2;
pub const DIGITAL_PORTA_HOTPLUG_ENABLE: u32 = bit(4);
pub const DIGITAL_PORTA_PULSE_DURATION_MASK: u32 = 3 << 2;
pub const DIGITAL_PORTA_HOTPLUG_LONG_DETECT: u32 = 2;
pub const DIGITAL_PORTA_PULSE_DURATION_2MS: u32 = 0;
pub const PORTA_HOTPLUG_STATUS_MASK: u32 = 3 << 24;
pub const PORTD_HOTPLUG_STATUS_MASK: u32 = 3 << 16;
pub const PORTC_HOTPLUG_STATUS_MASK: u32 = 3 << 8;
pub const PORTB_HOTPLUG_STATUS_MASK: u32 = 3;
pub const PORTE_HOTPLUG_STATUS_MASK: u32 = 3;

pub const SHPD_FILTER_CNT_500_ADJ: u32 = 0x001d9;
pub const SHPD_FILTER_CNT_250: u32 = 0x000f8;
pub const XELPDP_TBT_HOTPLUG_ENABLE: u32 = bit(6);
pub const XELPDP_TBT_HPD_LONG_DETECT: u32 = bit(5);
pub const XELPDP_DP_ALT_HOTPLUG_ENABLE: u32 = bit(2);
pub const XELPDP_DP_ALT_HPD_LONG_DETECT: u32 = bit(1);
pub const INVERT_DDIA_HPD: u32 = bit(15);
pub const INVERT_DDIB_HPD: u32 = bit(16);
pub const INVERT_DDIC_HPD: u32 = bit(17);
pub const INVERT_DDID_HPD: u32 = bit(18);
pub const INVERT_TC1_HPD: u32 = bit(23);
pub const INVERT_TC2_HPD: u32 = bit(24);
pub const INVERT_TC3_HPD: u32 = bit(25);
pub const INVERT_TC4_HPD: u32 = bit(26);
pub const INVERT_DDID_HPD_MTP: u32 = bit(27);
pub const INVERT_DDIE_HPD: u32 = bit(28);
pub const CHASSIS_CLK_REQ_DURATION_MASK: u32 = 0xf << 8;
pub const CHASSIS_CLK_REQ_DURATION_F: u32 = 0xf << 8;
pub const PEG_BAND_GAP_DATA_MASK: u32 = 0xf;
pub const PEG_BAND_GAP_DATA_VALUE: u32 = 0xd;
pub const GEN11_DE_TC_HOTPLUG_MASK: u32 = 0x3f << 16;
pub const GEN11_DE_TBT_HOTPLUG_MASK: u32 = 0x3f;
pub const XELPDP_DP_ALT_HOTPLUG_MASK: u32 = 0xf << 16;
pub const XELPDP_TBT_HOTPLUG_MASK: u32 = 0xf;
pub const SDE_GMBUS_ICP: u32 = bit(23);
pub const SDE_GMBUS_CPT: u32 = bit(17);
pub const SDE_HOTPLUG_MASK_SPT: u32 = (1 << 25) | (1 << 24) | (1 << 23) | (1 << 22) | (1 << 21);
pub const SDE_PORTE_HOTPLUG_SPT: u32 = bit(25);
pub const SDE_HOTPLUG_MASK_CPT: u32 = (1 << 23) | (1 << 22) | (1 << 21) | (1 << 19) | (1 << 18);
pub const SDE_DDI_HOTPLUG_MASK_ICP: u32 = bit(16) | bit(17) | bit(18) | bit(19);
pub const SDE_TC_HOTPLUG_MASK_ICP: u32 = 0x3f << 24;
pub const XELPDP_PICA_HPD_PIN_MIN: HpdPin = HPD_PORT_TC1;
pub const XELPDP_PICA_HPD_PIN_MAX: HpdPin = HPD_PORT_TC4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PchType {
    None,
    IbX,
    Cpt,
    Lpt,
    Spt,
    Cnp,
    Icp,
    Tgp,
    Adp,
    Dg1,
    Dg2,
    Mtl,
    Lnl,
    Other(u8),
}
impl PchType {
    fn rank(self) -> u8 {
        match self {
            Self::None => 0,
            Self::IbX => 1,
            Self::Cpt => 2,
            Self::Lpt => 3,
            Self::Spt => 4,
            Self::Cnp => 5,
            Self::Icp => 6,
            Self::Tgp => 7,
            Self::Adp => 8,
            Self::Dg1 => 9,
            Self::Dg2 => 10,
            Self::Mtl => 11,
            Self::Lnl => 12,
            Self::Other(n) => n,
        }
    }
    fn at_least(self, other: Self) -> bool {
        self.rank() >= other.rank()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HotplugFuncs {
    I915,
    XelPdp,
    Dg1,
    Gen11,
    Bxt,
    Icp,
    Spt,
    Ilk,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HpdState {
    Enabled,
    Disabled,
    MarkDisabled,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Encoder {
    pub hpd_pin: HpdPin,
    pub bios_hpd_invert: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Platform {
    pub gmch: bool,
    pub g4x: bool,
    pub g45: bool,
    pub valleyview: bool,
    pub cherryview: bool,
    pub geminilake: bool,
    pub broxton: bool,
    pub has_hotplug: bool,
    pub has_pch_split: bool,
    pub has_pch_nop: bool,
    pub has_pch_lpt_lp: bool,
    pub pch: PchType,
    pub display_ver: u8,
    pub vlv_display_irqs_enabled: bool,
}
impl Default for Platform {
    fn default() -> Self {
        Self {
            gmch: false,
            g4x: false,
            g45: false,
            valleyview: false,
            cherryview: false,
            geminilake: false,
            broxton: false,
            has_hotplug: false,
            has_pch_split: false,
            has_pch_nop: false,
            has_pch_lpt_lp: false,
            pch: PchType::None,
            display_ver: 0,
            vlv_display_irqs_enabled: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqRegister {
    PortHotplugEn,
    PortHotplugStat,
    PchPortHotplug,
    PchPortHotplug2,
    DigitalPortHotplugControl,
    Gen11TcHotplugControl,
    Gen11TbtHotplugControl,
    ShotplugDdi,
    ShotplugTc,
    ShpdFilterCount,
    SouthChicken1,
    PegBandGapData,
    Gen11DeHpdImr,
    PicaInterruptImr,
    XelpdpPortHotplugControl(HpdPin),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptBlock {
    IbX,
    BdwPort,
    IlkDisplay,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqLock {
    Plain,
    Irq,
    IrqSave,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqLog {
    HotplugEvent {
        status: u32,
        dig: u32,
        pins: u32,
        long: u32,
    },
    PicaHotplugEvent {
        status: u32,
        pins: u32,
        long: u32,
    },
    UnexpectedDeHpdAux(u32),
    UnexpectedDeHpd(u32),
    MissingPch(PchType),
}

/// All kernel/framework and hardware-specific dependencies are explicit hooks.
pub trait HotplugIrqIo {
    fn read(&mut self, reg: IrqRegister) -> u32;
    fn write(&mut self, reg: IrqRegister, value: u32);
    fn rmw(&mut self, reg: IrqRegister, clear: u32, set: u32) -> u32;
    fn posting_read(&mut self, reg: IrqRegister);
    fn lock(&mut self, lock: IrqLock);
    fn unlock(&mut self, lock: IrqLock);
    fn assert_lock_held(&mut self, lock: IrqLock);
    fn warn(&mut self, message: &'static str, value: u32);
    fn warn_once(&mut self, message: &'static str, value: u32);
    fn log(&mut self, event: IrqLog);
    fn hpd_irq_handler(&mut self, pin_mask: u32, long_mask: u32);
    fn dp_aux_irq_handler(&mut self);
    fn gmbus_irq_handler(&mut self);
    fn update_interrupts(&mut self, block: InterruptBlock, mask: u32, enabled: u32);
    fn hpd_init_early(&mut self);
    fn xelpdp_pica_aux_mask(&self) -> u32;
    fn bios_hpd_invert(&self, encoder: &Encoder) -> bool {
        encoder.bios_hpd_invert
    }
}

pub struct IntelHotplugIrq {
    pub platform: Platform,
    pub hpd: Option<[u32; HPD_NUM_PINS]>,
    pub pch_hpd: Option<[u32; HPD_NUM_PINS]>,
    /// Linux indexes HPD status by pin, not by encoder.
    pub hpd_state: [HpdState; HPD_NUM_PINS],
    pub encoders: alloc::vec::Vec<Encoder>,
    pub funcs: Option<HotplugFuncs>,
}

impl IntelHotplugIrq {
    pub fn new(platform: Platform, encoders: alloc::vec::Vec<Encoder>) -> Self {
        Self {
            platform,
            hpd: None,
            pch_hpd: None,
            hpd_state: [HpdState::Enabled; HPD_NUM_PINS],
            encoders,
            funcs: None,
        }
    }

    // upstream: intel_hotplug_irq.c intel_hpd_init_pins()
    fn intel_hpd_init_pins(&mut self, io: &mut impl HotplugIrqIo) {
        let p = self.platform;
        if p.gmch {
            self.hpd = Some(if p.g4x || p.valleyview || p.cherryview {
                hpd_g4x_status()
            } else {
                hpd_i915_status()
            });
            return;
        }
        self.hpd = if p.display_ver >= 14 {
            Some(hpd_xelpdp())
        } else if p.display_ver >= 11 {
            Some(hpd_gen11())
        } else if p.geminilake || p.broxton {
            Some(hpd_bxt())
        } else if p.display_ver == 9 {
            None
        } else if p.display_ver >= 8 {
            Some(hpd_bdw())
        } else if p.display_ver >= 7 {
            Some(hpd_ivb())
        } else {
            Some(hpd_ilk())
        };

        if p.pch.rank() < PchType::Dg1.rank() && (!p.has_pch_split || p.has_pch_nop) {
            return;
        }
        self.pch_hpd = if p.pch.at_least(PchType::Mtl) {
            Some(hpd_mtp())
        } else if p.pch.at_least(PchType::Dg1) {
            Some(hpd_sde_dg1())
        } else if p.pch.at_least(PchType::Icp) {
            Some(hpd_icp())
        } else if p.pch == PchType::Cnp || p.pch == PchType::Spt {
            Some(hpd_spt())
        } else if p.pch == PchType::Lpt || p.pch == PchType::Cpt {
            Some(hpd_cpt())
        } else if p.pch == PchType::IbX {
            Some(hpd_ibx())
        } else {
            io.log(IrqLog::MissingPch(p.pch));
            self.pch_hpd
        };
    }

    // upstream: intel_hotplug_irq.c i915_hotplug_interrupt_update_locked()
    pub fn i915_hotplug_interrupt_update_locked(
        &self,
        io: &mut impl HotplugIrqIo,
        mask: u32,
        bits: u32,
    ) {
        io.assert_lock_held(IrqLock::Irq);
        if bits & !mask != 0 {
            io.warn("hotplug enable bits outside update mask", bits & !mask);
        }
        io.rmw(IrqRegister::PortHotplugEn, mask, bits);
    }

    // upstream: intel_hotplug_irq.c i915_hotplug_interrupt_update()
    pub fn i915_hotplug_interrupt_update(&self, io: &mut impl HotplugIrqIo, mask: u32, bits: u32) {
        io.lock(IrqLock::Irq);
        self.i915_hotplug_interrupt_update_locked(io, mask, bits);
        io.unlock(IrqLock::Irq);
    }

    // upstream: intel_hotplug_irq.c gen11_port_hotplug_long_detect()
    fn gen11_port_hotplug_long_detect(pin: HpdPin, val: u32) -> bool {
        match pin {
            HPD_PORT_TC1..=HPD_PORT_TC6 => val & (2 << (tc_index(pin) * 4)) != 0,
            _ => false,
        }
    }

    // upstream: intel_hotplug_irq.c bxt_port_hotplug_long_detect()
    fn bxt_port_hotplug_long_detect(pin: HpdPin, val: u32) -> bool {
        match pin {
            HPD_PORT_A => val & PORTA_HOTPLUG_LONG_DETECT != 0,
            HPD_PORT_B => val & PORTB_HOTPLUG_LONG_DETECT != 0,
            HPD_PORT_C => val & PORTC_HOTPLUG_LONG_DETECT != 0,
            _ => false,
        }
    }

    // upstream: intel_hotplug_irq.c icp_ddi_port_hotplug_long_detect()
    fn icp_ddi_port_hotplug_long_detect(pin: HpdPin, val: u32) -> bool {
        match pin {
            HPD_PORT_A..=HPD_PORT_D => val & (2 << (ddi_index(pin) * 4)) != 0,
            _ => false,
        }
    }

    // upstream: intel_hotplug_irq.c icp_tc_port_hotplug_long_detect()
    fn icp_tc_port_hotplug_long_detect(pin: HpdPin, val: u32) -> bool {
        match pin {
            HPD_PORT_TC1..=HPD_PORT_TC6 => val & (2 << (tc_index(pin) * 4)) != 0,
            _ => false,
        }
    }

    // upstream: intel_hotplug_irq.c spt_port_hotplug2_long_detect()
    fn spt_port_hotplug2_long_detect(pin: HpdPin, val: u32) -> bool {
        match pin {
            HPD_PORT_E => val & PORTE_HOTPLUG_LONG_DETECT != 0,
            _ => false,
        }
    }

    // upstream: intel_hotplug_irq.c spt_port_hotplug_long_detect()
    fn spt_port_hotplug_long_detect(pin: HpdPin, val: u32) -> bool {
        match pin {
            HPD_PORT_A => val & PORTA_HOTPLUG_LONG_DETECT != 0,
            HPD_PORT_B => val & PORTB_HOTPLUG_LONG_DETECT != 0,
            HPD_PORT_C => val & PORTC_HOTPLUG_LONG_DETECT != 0,
            HPD_PORT_D => val & PORTD_HOTPLUG_LONG_DETECT != 0,
            _ => false,
        }
    }

    // upstream: intel_hotplug_irq.c ilk_port_hotplug_long_detect()
    fn ilk_port_hotplug_long_detect(pin: HpdPin, val: u32) -> bool {
        match pin {
            HPD_PORT_A => val & DIGITAL_PORTA_HOTPLUG_LONG_DETECT != 0,
            _ => false,
        }
    }

    // upstream: intel_hotplug_irq.c pch_port_hotplug_long_detect()
    fn pch_port_hotplug_long_detect(pin: HpdPin, val: u32) -> bool {
        match pin {
            HPD_PORT_B => val & PORTB_HOTPLUG_LONG_DETECT != 0,
            HPD_PORT_C => val & PORTC_HOTPLUG_LONG_DETECT != 0,
            HPD_PORT_D => val & PORTD_HOTPLUG_LONG_DETECT != 0,
            _ => false,
        }
    }

    // upstream: intel_hotplug_irq.c i9xx_port_hotplug_long_detect()
    fn i9xx_port_hotplug_long_detect(pin: HpdPin, val: u32) -> bool {
        match pin {
            HPD_PORT_B => val & (2 << 17) != 0,
            HPD_PORT_C => val & (2 << 19) != 0,
            HPD_PORT_D => val & (2 << 21) != 0,
            _ => false,
        }
    }

    // upstream: intel_hotplug_irq.c intel_get_hpd_pins()
    fn intel_get_hpd_pins(
        &self,
        io: &mut impl HotplugIrqIo,
        pin_mask: &mut u32,
        long_mask: &mut u32,
        hotplug_trigger: u32,
        dig_hotplug_reg: u32,
        hpd: &[u32; HPD_NUM_PINS],
        long_pulse_detect: fn(HpdPin, u32) -> bool,
    ) {
        for pin in 1..HPD_NUM_PINS as HpdPin {
            if hpd[pin as usize] & hotplug_trigger == 0 {
                continue;
            }
            *pin_mask |= pin_bit(pin);
            if long_pulse_detect(pin, dig_hotplug_reg) {
                *long_mask |= pin_bit(pin);
            }
        }
        io.log(IrqLog::HotplugEvent {
            status: hotplug_trigger,
            dig: dig_hotplug_reg,
            pins: *pin_mask,
            long: *long_mask,
        });
    }

    // upstream: intel_hotplug_irq.c intel_hpd_enabled_irqs()
    fn intel_hpd_enabled_irqs(&self, hpd: &[u32; HPD_NUM_PINS]) -> u32 {
        let mut enabled_irqs = 0;
        for encoder in &self.encoders {
            let pin = encoder.hpd_pin as usize;
            if pin < HPD_NUM_PINS && self.hpd_state[pin] == HpdState::Enabled {
                enabled_irqs |= hpd[pin];
            }
        }
        enabled_irqs
    }

    // upstream: intel_hotplug_irq.c intel_hpd_hotplug_irqs()
    fn intel_hpd_hotplug_irqs(&self, hpd: &[u32; HPD_NUM_PINS]) -> u32 {
        let mut hotplug_irqs = 0;
        for encoder in &self.encoders {
            if (encoder.hpd_pin as usize) < HPD_NUM_PINS {
                hotplug_irqs |= hpd[encoder.hpd_pin as usize];
            }
        }
        hotplug_irqs
    }

    // upstream: intel_hotplug_irq.c intel_hpd_hotplug_mask()
    fn intel_hpd_hotplug_mask(&self, hotplug_mask: fn(HpdPin) -> u32) -> u32 {
        let mut hotplug = 0;
        for pin in 1..HPD_NUM_PINS as HpdPin {
            hotplug |= hotplug_mask(pin);
        }
        hotplug
    }

    // upstream: intel_hotplug_irq.c intel_hpd_hotplug_enables()
    fn intel_hpd_hotplug_enables(&self, mut hotplug_enables: impl FnMut(&Encoder) -> u32) -> u32 {
        let mut hotplug = 0;
        for encoder in &self.encoders {
            hotplug |= hotplug_enables(encoder);
        }
        hotplug
    }

    // upstream: intel_hotplug_irq.c i9xx_hpd_irq_ack()
    pub fn i9xx_hpd_irq_ack(&self, io: &mut impl HotplugIrqIo) -> u32 {
        if !self.platform.has_hotplug {
            return 0;
        }
        let status_mask =
            if self.platform.g4x || self.platform.valleyview || self.platform.cherryview {
                HOTPLUG_INT_STATUS_G4X | DP_AUX_CHANNEL_MASK_INT_STATUS_G4X
            } else {
                HOTPLUG_INT_STATUS_I915
            };
        let mut hotplug_status = 0;
        for _ in 0..10 {
            let tmp = io.read(IrqRegister::PortHotplugStat) & status_mask;
            if tmp == 0 {
                return hotplug_status;
            }
            hotplug_status |= tmp;
            io.write(IrqRegister::PortHotplugStat, hotplug_status);
        }
        let pending = io.read(IrqRegister::PortHotplugStat);
        io.warn_once("PORT_HOTPLUG_STAT did not clear", pending);
        hotplug_status
    }

    // upstream: intel_hotplug_irq.c i9xx_hpd_irq_handler()
    pub fn i9xx_hpd_irq_handler(&self, io: &mut impl HotplugIrqIo, hotplug_status: u32) {
        let hotplug_trigger = hotplug_status
            & if self.platform.g4x || self.platform.valleyview || self.platform.cherryview {
                HOTPLUG_INT_STATUS_G4X
            } else {
                HOTPLUG_INT_STATUS_I915
            };
        if hotplug_trigger != 0 {
            let mut pins = 0;
            let mut longs = 0;
            self.intel_get_hpd_pins(
                io,
                &mut pins,
                &mut longs,
                hotplug_trigger,
                hotplug_trigger,
                self.hpd.as_ref().unwrap_or(&ZERO_HPD),
                Self::i9xx_port_hotplug_long_detect,
            );
            io.hpd_irq_handler(pins, longs);
        }
        if (self.platform.g4x || self.platform.valleyview || self.platform.cherryview)
            && hotplug_status & DP_AUX_CHANNEL_MASK_INT_STATUS_G4X != 0
        {
            io.dp_aux_irq_handler();
        }
    }

    // upstream: intel_hotplug_irq.c ibx_hpd_irq_handler()
    pub fn ibx_hpd_irq_handler(&self, io: &mut impl HotplugIrqIo, hotplug_trigger: u32) {
        let mut dig_hotplug_reg = io.read(IrqRegister::PchPortHotplug);
        if hotplug_trigger == 0 {
            dig_hotplug_reg &= !(PORTA_HOTPLUG_STATUS_MASK
                | PORTD_HOTPLUG_STATUS_MASK
                | PORTC_HOTPLUG_STATUS_MASK
                | PORTB_HOTPLUG_STATUS_MASK);
        }
        io.write(IrqRegister::PchPortHotplug, dig_hotplug_reg);
        if hotplug_trigger == 0 {
            return;
        }
        let mut pins = 0;
        let mut longs = 0;
        self.intel_get_hpd_pins(
            io,
            &mut pins,
            &mut longs,
            hotplug_trigger,
            dig_hotplug_reg,
            self.pch_hpd.as_ref().unwrap_or(&ZERO_HPD),
            Self::pch_port_hotplug_long_detect,
        );
        io.hpd_irq_handler(pins, longs);
    }

    // upstream: intel_hotplug_irq.c xelpdp_pica_irq_handler()
    pub fn xelpdp_pica_irq_handler(&self, io: &mut impl HotplugIrqIo, iir: u32) {
        let hotplug_trigger = iir & (XELPDP_DP_ALT_HOTPLUG_MASK | XELPDP_TBT_HOTPLUG_MASK);
        let trigger_aux = iir & io.xelpdp_pica_aux_mask();
        let mut pins = 0;
        let mut longs = 0;
        for pin in HPD_PORT_TC1..=HPD_PORT_TC4 {
            if self.hpd.as_ref().unwrap_or(&ZERO_HPD)[pin as usize] & hotplug_trigger == 0 {
                continue;
            }
            pins |= pin_bit(pin);
            let val = io.read(IrqRegister::XelpdpPortHotplugControl(pin));
            io.write(IrqRegister::XelpdpPortHotplugControl(pin), val);
            if val & (XELPDP_DP_ALT_HPD_LONG_DETECT | XELPDP_TBT_HPD_LONG_DETECT) != 0 {
                longs |= pin_bit(pin);
            }
        }
        if pins != 0 {
            io.log(IrqLog::PicaHotplugEvent {
                status: hotplug_trigger,
                pins,
                long: longs,
            });
            io.hpd_irq_handler(pins, longs);
        }
        if trigger_aux != 0 {
            io.dp_aux_irq_handler();
        }
        if pins == 0 && trigger_aux == 0 {
            io.log(IrqLog::UnexpectedDeHpdAux(iir));
        }
    }

    // upstream: intel_hotplug_irq.c icp_irq_handler()
    pub fn icp_irq_handler(&self, io: &mut impl HotplugIrqIo, pch_iir: u32) {
        let ddi_trigger = pch_iir & SDE_DDI_HOTPLUG_MASK_ICP;
        let tc_trigger = pch_iir & SDE_TC_HOTPLUG_MASK_ICP;
        let mut pins = 0;
        let mut longs = 0;
        if ddi_trigger != 0 {
            io.lock(IrqLock::Plain);
            let reg = io.rmw(IrqRegister::ShotplugDdi, 0, 0);
            io.unlock(IrqLock::Plain);
            self.intel_get_hpd_pins(
                io,
                &mut pins,
                &mut longs,
                ddi_trigger,
                reg,
                self.pch_hpd.as_ref().unwrap_or(&ZERO_HPD),
                Self::icp_ddi_port_hotplug_long_detect,
            );
        }
        if tc_trigger != 0 {
            let reg = io.rmw(IrqRegister::ShotplugTc, 0, 0);
            self.intel_get_hpd_pins(
                io,
                &mut pins,
                &mut longs,
                tc_trigger,
                reg,
                self.pch_hpd.as_ref().unwrap_or(&ZERO_HPD),
                Self::icp_tc_port_hotplug_long_detect,
            );
        }
        if pins != 0 {
            io.hpd_irq_handler(pins, longs);
        }
        if pch_iir & SDE_GMBUS_ICP != 0 {
            io.gmbus_irq_handler();
        }
    }

    // upstream: intel_hotplug_irq.c spt_irq_handler()
    pub fn spt_irq_handler(&self, io: &mut impl HotplugIrqIo, pch_iir: u32) {
        let hotplug_trigger = pch_iir & SDE_HOTPLUG_MASK_SPT & !SDE_PORTE_HOTPLUG_SPT;
        let hotplug2_trigger = pch_iir & SDE_PORTE_HOTPLUG_SPT;
        let mut pins = 0;
        let mut longs = 0;
        if hotplug_trigger != 0 {
            let reg = io.rmw(IrqRegister::PchPortHotplug, 0, 0);
            self.intel_get_hpd_pins(
                io,
                &mut pins,
                &mut longs,
                hotplug_trigger,
                reg,
                self.pch_hpd.as_ref().unwrap_or(&ZERO_HPD),
                Self::spt_port_hotplug_long_detect,
            );
        }
        if hotplug2_trigger != 0 {
            let reg = io.rmw(IrqRegister::PchPortHotplug2, 0, 0);
            self.intel_get_hpd_pins(
                io,
                &mut pins,
                &mut longs,
                hotplug2_trigger,
                reg,
                self.pch_hpd.as_ref().unwrap_or(&ZERO_HPD),
                Self::spt_port_hotplug2_long_detect,
            );
        }
        if pins != 0 {
            io.hpd_irq_handler(pins, longs);
        }
        if pch_iir & SDE_GMBUS_CPT != 0 {
            io.gmbus_irq_handler();
        }
    }

    // upstream: intel_hotplug_irq.c ilk_hpd_irq_handler()
    pub fn ilk_hpd_irq_handler(&self, io: &mut impl HotplugIrqIo, hotplug_trigger: u32) {
        let reg = io.rmw(IrqRegister::DigitalPortHotplugControl, 0, 0);
        let mut pins = 0;
        let mut longs = 0;
        self.intel_get_hpd_pins(
            io,
            &mut pins,
            &mut longs,
            hotplug_trigger,
            reg,
            self.hpd.as_ref().unwrap_or(&ZERO_HPD),
            Self::ilk_port_hotplug_long_detect,
        );
        io.hpd_irq_handler(pins, longs);
    }

    // upstream: intel_hotplug_irq.c bxt_hpd_irq_handler()
    pub fn bxt_hpd_irq_handler(&self, io: &mut impl HotplugIrqIo, hotplug_trigger: u32) {
        let reg = io.rmw(IrqRegister::PchPortHotplug, 0, 0);
        let mut pins = 0;
        let mut longs = 0;
        self.intel_get_hpd_pins(
            io,
            &mut pins,
            &mut longs,
            hotplug_trigger,
            reg,
            self.hpd.as_ref().unwrap_or(&ZERO_HPD),
            Self::bxt_port_hotplug_long_detect,
        );
        io.hpd_irq_handler(pins, longs);
    }

    // upstream: intel_hotplug_irq.c gen11_hpd_irq_handler()
    pub fn gen11_hpd_irq_handler(&self, io: &mut impl HotplugIrqIo, iir: u32) {
        let mut pins = 0;
        let mut longs = 0;
        let trigger_tc = iir & GEN11_DE_TC_HOTPLUG_MASK;
        let trigger_tbt = iir & GEN11_DE_TBT_HOTPLUG_MASK;
        if trigger_tc != 0 {
            let reg = io.rmw(IrqRegister::Gen11TcHotplugControl, 0, 0);
            self.intel_get_hpd_pins(
                io,
                &mut pins,
                &mut longs,
                trigger_tc,
                reg,
                self.hpd.as_ref().unwrap_or(&ZERO_HPD),
                Self::gen11_port_hotplug_long_detect,
            );
        }
        if trigger_tbt != 0 {
            let reg = io.rmw(IrqRegister::Gen11TbtHotplugControl, 0, 0);
            self.intel_get_hpd_pins(
                io,
                &mut pins,
                &mut longs,
                trigger_tbt,
                reg,
                self.hpd.as_ref().unwrap_or(&ZERO_HPD),
                Self::gen11_port_hotplug_long_detect,
            );
        }
        if pins != 0 {
            io.hpd_irq_handler(pins, longs);
        } else {
            io.log(IrqLog::UnexpectedDeHpd(iir));
        }
    }

    // upstream: intel_hotplug_irq.c ibx_hotplug_mask()
    fn ibx_hotplug_mask(pin: HpdPin) -> u32 {
        match pin {
            HPD_PORT_A => PORTA_HOTPLUG_ENABLE,
            HPD_PORT_B => PORTB_HOTPLUG_ENABLE | PORTB_PULSE_DURATION_MASK,
            HPD_PORT_C => PORTC_HOTPLUG_ENABLE | PORTC_PULSE_DURATION_MASK,
            HPD_PORT_D => PORTD_HOTPLUG_ENABLE | PORTD_PULSE_DURATION_MASK,
            _ => 0,
        }
    }

    // upstream: intel_hotplug_irq.c ibx_hotplug_enables()
    fn ibx_hotplug_enables(&self, encoder: &Encoder) -> u32 {
        match encoder.hpd_pin {
            HPD_PORT_A => {
                if self.platform.has_pch_lpt_lp {
                    PORTA_HOTPLUG_ENABLE
                } else {
                    0
                }
            }
            HPD_PORT_B => PORTB_HOTPLUG_ENABLE | PORTB_PULSE_DURATION_2MS,
            HPD_PORT_C => PORTC_HOTPLUG_ENABLE | PORTC_PULSE_DURATION_2MS,
            HPD_PORT_D => PORTD_HOTPLUG_ENABLE | PORTD_PULSE_DURATION_2MS,
            _ => 0,
        }
    }

    // upstream: intel_hotplug_irq.c ibx_hpd_detection_setup()
    fn ibx_hpd_detection_setup(&self, io: &mut impl HotplugIrqIo) {
        let mask = self.intel_hpd_hotplug_mask(Self::ibx_hotplug_mask);
        let enables = self.intel_hpd_hotplug_enables(|e| self.ibx_hotplug_enables(e));
        io.rmw(IrqRegister::PchPortHotplug, mask, enables);
    }

    // upstream: intel_hotplug_irq.c ibx_hpd_enable_detection()
    fn ibx_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        io.rmw(
            IrqRegister::PchPortHotplug,
            Self::ibx_hotplug_mask(encoder.hpd_pin),
            self.ibx_hotplug_enables(encoder),
        );
    }

    // upstream: intel_hotplug_irq.c ibx_hpd_irq_setup()
    fn ibx_hpd_irq_setup(&self, io: &mut impl HotplugIrqIo) {
        let hpd = self.pch_hpd.as_ref().unwrap_or(&ZERO_HPD);
        let enabled = self.intel_hpd_enabled_irqs(hpd);
        let hotplug = self.intel_hpd_hotplug_irqs(hpd);
        io.update_interrupts(InterruptBlock::IbX, hotplug, enabled);
        self.ibx_hpd_detection_setup(io);
    }

    // upstream: intel_hotplug_irq.c icp_ddi_hotplug_mask()
    fn icp_ddi_hotplug_mask(pin: HpdPin) -> u32 {
        match pin {
            HPD_PORT_A..=HPD_PORT_D => 8 << (ddi_index(pin) * 4),
            _ => 0,
        }
    }

    // upstream: intel_hotplug_irq.c icp_ddi_hotplug_enables()
    fn icp_ddi_hotplug_enables(encoder: &Encoder) -> u32 {
        Self::icp_ddi_hotplug_mask(encoder.hpd_pin)
    }

    // upstream: intel_hotplug_irq.c icp_tc_hotplug_mask()
    fn icp_tc_hotplug_mask(pin: HpdPin) -> u32 {
        match pin {
            HPD_PORT_TC1..=HPD_PORT_TC6 => 8 << (tc_index(pin) * 4),
            _ => 0,
        }
    }

    // upstream: intel_hotplug_irq.c icp_tc_hotplug_enables()
    fn icp_tc_hotplug_enables(encoder: &Encoder) -> u32 {
        Self::icp_tc_hotplug_mask(encoder.hpd_pin)
    }

    // upstream: intel_hotplug_irq.c icp_ddi_hpd_detection_setup()
    fn icp_ddi_hpd_detection_setup(&self, io: &mut impl HotplugIrqIo) {
        let mask = self.intel_hpd_hotplug_mask(Self::icp_ddi_hotplug_mask);
        let enables = self.intel_hpd_hotplug_enables(Self::icp_ddi_hotplug_enables);
        io.rmw(IrqRegister::ShotplugDdi, mask, enables);
    }

    // upstream: intel_hotplug_irq.c icp_ddi_hpd_enable_detection()
    fn icp_ddi_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        io.rmw(
            IrqRegister::ShotplugDdi,
            Self::icp_ddi_hotplug_mask(encoder.hpd_pin),
            Self::icp_ddi_hotplug_enables(encoder),
        );
    }

    // upstream: intel_hotplug_irq.c icp_tc_hpd_detection_setup()
    fn icp_tc_hpd_detection_setup(&self, io: &mut impl HotplugIrqIo) {
        let mask = self.intel_hpd_hotplug_mask(Self::icp_tc_hotplug_mask);
        let enables = self.intel_hpd_hotplug_enables(Self::icp_tc_hotplug_enables);
        io.rmw(IrqRegister::ShotplugTc, mask, enables);
    }

    // upstream: intel_hotplug_irq.c icp_tc_hpd_enable_detection()
    fn icp_tc_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        io.rmw(
            IrqRegister::ShotplugTc,
            Self::icp_tc_hotplug_mask(encoder.hpd_pin),
            Self::icp_tc_hotplug_enables(encoder),
        );
    }

    // upstream: intel_hotplug_irq.c icp_hpd_enable_detection()
    fn icp_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        self.icp_ddi_hpd_enable_detection(io, encoder);
        self.icp_tc_hpd_enable_detection(io, encoder);
    }

    // upstream: intel_hotplug_irq.c icp_hpd_irq_setup()
    fn icp_hpd_irq_setup(&self, io: &mut impl HotplugIrqIo) {
        let hpd = self.pch_hpd.as_ref().unwrap_or(&ZERO_HPD);
        let enabled = self.intel_hpd_enabled_irqs(hpd);
        let hotplug = self.intel_hpd_hotplug_irqs(hpd);
        io.write(IrqRegister::ShpdFilterCount, SHPD_FILTER_CNT_250);
        io.update_interrupts(InterruptBlock::IbX, hotplug, enabled);
        self.icp_ddi_hpd_detection_setup(io);
        self.icp_tc_hpd_detection_setup(io);
    }

    // upstream: intel_hotplug_irq.c gen11_hotplug_mask()
    fn gen11_hotplug_mask(pin: HpdPin) -> u32 {
        match pin {
            HPD_PORT_TC1..=HPD_PORT_TC6 => 8 << (tc_index(pin) * 4),
            _ => 0,
        }
    }

    // upstream: intel_hotplug_irq.c gen11_hotplug_enables()
    fn gen11_hotplug_enables(encoder: &Encoder) -> u32 {
        Self::gen11_hotplug_mask(encoder.hpd_pin)
    }

    // upstream: intel_hotplug_irq.c dg1_hpd_invert()
    fn dg1_hpd_invert(&self, io: &mut impl HotplugIrqIo) {
        let val = INVERT_DDIA_HPD | INVERT_DDIB_HPD | INVERT_DDIC_HPD | INVERT_DDID_HPD;
        io.rmw(IrqRegister::SouthChicken1, 0, val);
    }

    // upstream: intel_hotplug_irq.c dg1_hpd_enable_detection()
    fn dg1_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        self.dg1_hpd_invert(io);
        self.icp_hpd_enable_detection(io, encoder);
    }

    // upstream: intel_hotplug_irq.c dg1_hpd_irq_setup()
    fn dg1_hpd_irq_setup(&self, io: &mut impl HotplugIrqIo) {
        self.dg1_hpd_invert(io);
        self.icp_hpd_irq_setup(io);
    }

    // upstream: intel_hotplug_irq.c gen11_tc_hpd_detection_setup()
    fn gen11_tc_hpd_detection_setup(&self, io: &mut impl HotplugIrqIo) {
        let mask = self.intel_hpd_hotplug_mask(Self::gen11_hotplug_mask);
        let enables = self.intel_hpd_hotplug_enables(Self::gen11_hotplug_enables);
        io.rmw(IrqRegister::Gen11TcHotplugControl, mask, enables);
    }

    // upstream: intel_hotplug_irq.c gen11_tc_hpd_enable_detection()
    fn gen11_tc_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        io.rmw(
            IrqRegister::Gen11TcHotplugControl,
            Self::gen11_hotplug_mask(encoder.hpd_pin),
            Self::gen11_hotplug_enables(encoder),
        );
    }

    // upstream: intel_hotplug_irq.c gen11_tbt_hpd_detection_setup()
    fn gen11_tbt_hpd_detection_setup(&self, io: &mut impl HotplugIrqIo) {
        let mask = self.intel_hpd_hotplug_mask(Self::gen11_hotplug_mask);
        let enables = self.intel_hpd_hotplug_enables(Self::gen11_hotplug_enables);
        io.rmw(IrqRegister::Gen11TbtHotplugControl, mask, enables);
    }

    // upstream: intel_hotplug_irq.c gen11_tbt_hpd_enable_detection()
    fn gen11_tbt_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        io.rmw(
            IrqRegister::Gen11TbtHotplugControl,
            Self::gen11_hotplug_mask(encoder.hpd_pin),
            Self::gen11_hotplug_enables(encoder),
        );
    }

    // upstream: intel_hotplug_irq.c gen11_hpd_enable_detection()
    fn gen11_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        self.gen11_tc_hpd_enable_detection(io, encoder);
        self.gen11_tbt_hpd_enable_detection(io, encoder);
        if self.platform.pch.at_least(PchType::Icp) {
            self.icp_hpd_enable_detection(io, encoder);
        }
    }

    // upstream: intel_hotplug_irq.c gen11_hpd_irq_setup()
    fn gen11_hpd_irq_setup(&self, io: &mut impl HotplugIrqIo) {
        let hpd = self.hpd.as_ref().unwrap_or(&ZERO_HPD);
        let enabled = self.intel_hpd_enabled_irqs(hpd);
        let hotplug = self.intel_hpd_hotplug_irqs(hpd);
        io.rmw(IrqRegister::Gen11DeHpdImr, hotplug, !enabled & hotplug);
        io.posting_read(IrqRegister::Gen11DeHpdImr);
        self.gen11_tc_hpd_detection_setup(io);
        self.gen11_tbt_hpd_detection_setup(io);
        if self.platform.pch.at_least(PchType::Icp) {
            self.icp_hpd_irq_setup(io);
        }
    }

    // upstream: intel_hotplug_irq.c mtp_ddi_hotplug_mask()
    fn mtp_ddi_hotplug_mask(pin: HpdPin) -> u32 {
        match pin {
            HPD_PORT_A | HPD_PORT_B => 8 << (ddi_index(pin) * 4),
            _ => 0,
        }
    }

    // upstream: intel_hotplug_irq.c mtp_ddi_hotplug_enables()
    fn mtp_ddi_hotplug_enables(encoder: &Encoder) -> u32 {
        Self::mtp_ddi_hotplug_mask(encoder.hpd_pin)
    }

    // upstream: intel_hotplug_irq.c mtp_tc_hotplug_mask()
    fn mtp_tc_hotplug_mask(pin: HpdPin) -> u32 {
        match pin {
            HPD_PORT_TC1..=HPD_PORT_TC4 => 8 << (tc_index(pin) * 4),
            _ => 0,
        }
    }

    // upstream: intel_hotplug_irq.c mtp_tc_hotplug_enables()
    fn mtp_tc_hotplug_enables(encoder: &Encoder) -> u32 {
        Self::mtp_tc_hotplug_mask(encoder.hpd_pin)
    }

    // upstream: intel_hotplug_irq.c mtp_ddi_hpd_detection_setup()
    fn mtp_ddi_hpd_detection_setup(&self, io: &mut impl HotplugIrqIo) {
        let mask = self.intel_hpd_hotplug_mask(Self::mtp_ddi_hotplug_mask);
        let enables = self.intel_hpd_hotplug_enables(Self::mtp_ddi_hotplug_enables);
        io.rmw(IrqRegister::ShotplugDdi, mask, enables);
    }

    // upstream: intel_hotplug_irq.c mtp_ddi_hpd_enable_detection()
    fn mtp_ddi_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        io.rmw(
            IrqRegister::ShotplugDdi,
            Self::mtp_ddi_hotplug_mask(encoder.hpd_pin),
            Self::mtp_ddi_hotplug_enables(encoder),
        );
    }

    // upstream: intel_hotplug_irq.c mtp_tc_hpd_detection_setup()
    fn mtp_tc_hpd_detection_setup(&self, io: &mut impl HotplugIrqIo) {
        let mask = self.intel_hpd_hotplug_mask(Self::mtp_tc_hotplug_mask);
        let enables = self.intel_hpd_hotplug_enables(Self::mtp_tc_hotplug_enables);
        io.rmw(IrqRegister::ShotplugTc, mask, enables);
    }

    // upstream: intel_hotplug_irq.c mtp_tc_hpd_enable_detection()
    fn mtp_tc_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        io.rmw(
            IrqRegister::ShotplugTc,
            Self::mtp_tc_hotplug_mask(encoder.hpd_pin),
            Self::mtp_tc_hotplug_enables(encoder),
        );
    }

    // upstream: intel_hotplug_irq.c mtp_hpd_invert()
    fn mtp_hpd_invert(&self, io: &mut impl HotplugIrqIo) {
        let val = INVERT_DDIA_HPD
            | INVERT_DDIB_HPD
            | INVERT_DDIC_HPD
            | INVERT_TC1_HPD
            | INVERT_TC2_HPD
            | INVERT_TC3_HPD
            | INVERT_TC4_HPD
            | INVERT_DDID_HPD_MTP
            | INVERT_DDIE_HPD;
        io.rmw(IrqRegister::SouthChicken1, 0, val);
    }

    // upstream: intel_hotplug_irq.c mtp_hpd_enable_detection()
    fn mtp_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        self.mtp_hpd_invert(io);
        self.mtp_ddi_hpd_enable_detection(io, encoder);
        self.mtp_tc_hpd_enable_detection(io, encoder);
    }

    // upstream: intel_hotplug_irq.c mtp_hpd_irq_setup()
    fn mtp_hpd_irq_setup(&self, io: &mut impl HotplugIrqIo) {
        let hpd = self.pch_hpd.as_ref().unwrap_or(&ZERO_HPD);
        let enabled = self.intel_hpd_enabled_irqs(hpd);
        let hotplug = self.intel_hpd_hotplug_irqs(hpd);
        io.write(IrqRegister::ShpdFilterCount, SHPD_FILTER_CNT_250);
        self.mtp_hpd_invert(io);
        io.update_interrupts(InterruptBlock::IbX, hotplug, enabled);
        self.mtp_ddi_hpd_detection_setup(io);
        self.mtp_tc_hpd_detection_setup(io);
    }

    // upstream: intel_hotplug_irq.c xe2lpd_sde_hpd_irq_setup()
    fn xe2lpd_sde_hpd_irq_setup(&self, io: &mut impl HotplugIrqIo) {
        let hpd = self.pch_hpd.as_ref().unwrap_or(&ZERO_HPD);
        let enabled = self.intel_hpd_enabled_irqs(hpd);
        let hotplug = self.intel_hpd_hotplug_irqs(hpd);
        io.update_interrupts(InterruptBlock::IbX, hotplug, enabled);
        self.mtp_ddi_hpd_detection_setup(io);
        self.mtp_tc_hpd_detection_setup(io);
    }

    // upstream: intel_hotplug_irq.c is_xelpdp_pica_hpd_pin()
    fn is_xelpdp_pica_hpd_pin(pin: HpdPin) -> bool {
        (HPD_PORT_TC1..=HPD_PORT_TC4).contains(&pin)
    }

    // upstream: intel_hotplug_irq.c _xelpdp_pica_hpd_detection_setup()
    fn _xelpdp_pica_hpd_detection_setup(
        &self,
        io: &mut impl HotplugIrqIo,
        pin: HpdPin,
        enable: bool,
    ) {
        let mask = XELPDP_TBT_HOTPLUG_ENABLE | XELPDP_DP_ALT_HOTPLUG_ENABLE;
        if !Self::is_xelpdp_pica_hpd_pin(pin) {
            return;
        }
        io.rmw(
            IrqRegister::XelpdpPortHotplugControl(pin),
            mask,
            if enable { mask } else { 0 },
        );
    }

    // upstream: intel_hotplug_irq.c xelpdp_pica_hpd_enable_detection()
    fn xelpdp_pica_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        self._xelpdp_pica_hpd_detection_setup(io, encoder.hpd_pin, true);
    }

    // upstream: intel_hotplug_irq.c xelpdp_pica_hpd_detection_setup()
    fn xelpdp_pica_hpd_detection_setup(&self, io: &mut impl HotplugIrqIo) {
        let mut available_pins = 0;
        for encoder in &self.encoders {
            available_pins |= pin_bit(encoder.hpd_pin);
        }
        for pin in 1..HPD_NUM_PINS as HpdPin {
            self._xelpdp_pica_hpd_detection_setup(io, pin, available_pins & pin_bit(pin) != 0);
        }
    }

    // upstream: intel_hotplug_irq.c xelpdp_hpd_enable_detection()
    fn xelpdp_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        self.xelpdp_pica_hpd_enable_detection(io, encoder);
        self.mtp_hpd_enable_detection(io, encoder);
    }

    // upstream: intel_hotplug_irq.c xelpdp_hpd_irq_setup()
    fn xelpdp_hpd_irq_setup(&self, io: &mut impl HotplugIrqIo) {
        let hpd = self.hpd.as_ref().unwrap_or(&ZERO_HPD);
        let enabled = self.intel_hpd_enabled_irqs(hpd);
        let hotplug = self.intel_hpd_hotplug_irqs(hpd);
        io.rmw(IrqRegister::PicaInterruptImr, hotplug, !enabled & hotplug);
        io.posting_read(IrqRegister::PicaInterruptImr);
        self.xelpdp_pica_hpd_detection_setup(io);
        if self.platform.pch.at_least(PchType::Lnl) {
            self.xe2lpd_sde_hpd_irq_setup(io);
        } else if self.platform.pch.at_least(PchType::Mtl) {
            self.mtp_hpd_irq_setup(io);
        }
    }

    // upstream: intel_hotplug_irq.c spt_hotplug_mask()
    fn spt_hotplug_mask(pin: HpdPin) -> u32 {
        match pin {
            HPD_PORT_A => PORTA_HOTPLUG_ENABLE,
            HPD_PORT_B => PORTB_HOTPLUG_ENABLE,
            HPD_PORT_C => PORTC_HOTPLUG_ENABLE,
            HPD_PORT_D => PORTD_HOTPLUG_ENABLE,
            _ => 0,
        }
    }

    // upstream: intel_hotplug_irq.c spt_hotplug_enables()
    fn spt_hotplug_enables(encoder: &Encoder) -> u32 {
        Self::spt_hotplug_mask(encoder.hpd_pin)
    }

    // upstream: intel_hotplug_irq.c spt_hotplug2_mask()
    fn spt_hotplug2_mask(pin: HpdPin) -> u32 {
        if pin == HPD_PORT_E {
            PORTE_HOTPLUG_ENABLE
        } else {
            0
        }
    }

    // upstream: intel_hotplug_irq.c spt_hotplug2_enables()
    fn spt_hotplug2_enables(encoder: &Encoder) -> u32 {
        Self::spt_hotplug2_mask(encoder.hpd_pin)
    }

    // upstream: intel_hotplug_irq.c spt_hpd_detection_setup()
    fn spt_hpd_detection_setup(&self, io: &mut impl HotplugIrqIo) {
        if self.platform.pch == PchType::Cnp {
            io.rmw(
                IrqRegister::SouthChicken1,
                CHASSIS_CLK_REQ_DURATION_MASK,
                CHASSIS_CLK_REQ_DURATION_F,
            );
        }
        let mask = self.intel_hpd_hotplug_mask(Self::spt_hotplug_mask);
        let enables = self.intel_hpd_hotplug_enables(Self::spt_hotplug_enables);
        io.rmw(IrqRegister::PchPortHotplug, mask, enables);
        let mask2 = self.intel_hpd_hotplug_mask(Self::spt_hotplug2_mask);
        let enables2 = self.intel_hpd_hotplug_enables(Self::spt_hotplug2_enables);
        io.rmw(IrqRegister::PchPortHotplug2, mask2, enables2);
    }

    // upstream: intel_hotplug_irq.c spt_hpd_enable_detection()
    fn spt_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        if self.platform.pch == PchType::Cnp {
            io.rmw(
                IrqRegister::SouthChicken1,
                CHASSIS_CLK_REQ_DURATION_MASK,
                CHASSIS_CLK_REQ_DURATION_F,
            );
        }
        io.rmw(
            IrqRegister::PchPortHotplug,
            Self::spt_hotplug_mask(encoder.hpd_pin),
            Self::spt_hotplug_enables(encoder),
        );
        io.rmw(
            IrqRegister::PchPortHotplug2,
            Self::spt_hotplug2_mask(encoder.hpd_pin),
            Self::spt_hotplug2_enables(encoder),
        );
    }

    // upstream: intel_hotplug_irq.c spt_hpd_irq_setup()
    fn spt_hpd_irq_setup(&self, io: &mut impl HotplugIrqIo) {
        if self.platform.pch.at_least(PchType::Cnp) {
            io.write(IrqRegister::ShpdFilterCount, SHPD_FILTER_CNT_500_ADJ);
        }
        let hpd = self.pch_hpd.as_ref().unwrap_or(&ZERO_HPD);
        let enabled = self.intel_hpd_enabled_irqs(hpd);
        let hotplug = self.intel_hpd_hotplug_irqs(hpd);
        io.update_interrupts(InterruptBlock::IbX, hotplug, enabled);
        self.spt_hpd_detection_setup(io);
    }

    // upstream: intel_hotplug_irq.c ilk_hotplug_mask()
    fn ilk_hotplug_mask(pin: HpdPin) -> u32 {
        match pin {
            HPD_PORT_A => DIGITAL_PORTA_HOTPLUG_ENABLE | DIGITAL_PORTA_PULSE_DURATION_MASK,
            _ => 0,
        }
    }

    // upstream: intel_hotplug_irq.c ilk_hotplug_enables()
    fn ilk_hotplug_enables(encoder: &Encoder) -> u32 {
        if encoder.hpd_pin == HPD_PORT_A {
            DIGITAL_PORTA_HOTPLUG_ENABLE | DIGITAL_PORTA_PULSE_DURATION_2MS
        } else {
            0
        }
    }

    // upstream: intel_hotplug_irq.c ilk_hpd_detection_setup()
    fn ilk_hpd_detection_setup(&self, io: &mut impl HotplugIrqIo) {
        let mask = self.intel_hpd_hotplug_mask(Self::ilk_hotplug_mask);
        let enables = self.intel_hpd_hotplug_enables(Self::ilk_hotplug_enables);
        io.rmw(IrqRegister::DigitalPortHotplugControl, mask, enables);
    }

    // upstream: intel_hotplug_irq.c ilk_hpd_enable_detection()
    fn ilk_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        io.rmw(
            IrqRegister::DigitalPortHotplugControl,
            Self::ilk_hotplug_mask(encoder.hpd_pin),
            Self::ilk_hotplug_enables(encoder),
        );
        self.ibx_hpd_enable_detection(io, encoder);
    }

    // upstream: intel_hotplug_irq.c ilk_hpd_irq_setup()
    fn ilk_hpd_irq_setup(&self, io: &mut impl HotplugIrqIo) {
        let hpd = self.hpd.as_ref().unwrap_or(&ZERO_HPD);
        let enabled = self.intel_hpd_enabled_irqs(hpd);
        let hotplug = self.intel_hpd_hotplug_irqs(hpd);
        if self.platform.display_ver >= 8 {
            io.update_interrupts(InterruptBlock::BdwPort, hotplug, enabled);
        } else {
            io.update_interrupts(InterruptBlock::IlkDisplay, hotplug, enabled);
        }
        self.ilk_hpd_detection_setup(io);
        self.ibx_hpd_irq_setup(io);
    }

    // upstream: intel_hotplug_irq.c bxt_hotplug_mask()
    fn bxt_hotplug_mask(pin: HpdPin) -> u32 {
        match pin {
            HPD_PORT_A => PORTA_HOTPLUG_ENABLE | BXT_DDIA_HPD_INVERT,
            HPD_PORT_B => PORTB_HOTPLUG_ENABLE | BXT_DDIB_HPD_INVERT,
            HPD_PORT_C => PORTC_HOTPLUG_ENABLE | BXT_DDIC_HPD_INVERT,
            _ => 0,
        }
    }

    // upstream: intel_hotplug_irq.c bxt_hotplug_enables()
    fn bxt_hotplug_enables(&self, io: &impl HotplugIrqIo, encoder: &Encoder) -> u32 {
        let (enable, invert) = match encoder.hpd_pin {
            HPD_PORT_A => (PORTA_HOTPLUG_ENABLE, BXT_DDIA_HPD_INVERT),
            HPD_PORT_B => (PORTB_HOTPLUG_ENABLE, BXT_DDIB_HPD_INVERT),
            HPD_PORT_C => (PORTC_HOTPLUG_ENABLE, BXT_DDIC_HPD_INVERT),
            _ => return 0,
        };
        if io.bios_hpd_invert(encoder) {
            enable | invert
        } else {
            enable
        }
    }

    // upstream: intel_hotplug_irq.c bxt_hpd_detection_setup()
    fn bxt_hpd_detection_setup(&self, io: &mut impl HotplugIrqIo) {
        let mask = self.intel_hpd_hotplug_mask(Self::bxt_hotplug_mask);
        let enables = self.intel_hpd_hotplug_enables(|e| self.bxt_hotplug_enables(io, e));
        io.rmw(IrqRegister::PchPortHotplug, mask, enables);
    }

    // upstream: intel_hotplug_irq.c bxt_hpd_enable_detection()
    fn bxt_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        io.rmw(
            IrqRegister::PchPortHotplug,
            Self::bxt_hotplug_mask(encoder.hpd_pin),
            self.bxt_hotplug_enables(io, encoder),
        );
    }

    // upstream: intel_hotplug_irq.c bxt_hpd_irq_setup()
    fn bxt_hpd_irq_setup(&self, io: &mut impl HotplugIrqIo) {
        let hpd = self.hpd.as_ref().unwrap_or(&ZERO_HPD);
        let enabled = self.intel_hpd_enabled_irqs(hpd);
        let hotplug = self.intel_hpd_hotplug_irqs(hpd);
        io.update_interrupts(InterruptBlock::BdwPort, hotplug, enabled);
        self.bxt_hpd_detection_setup(io);
    }

    // upstream: intel_hotplug_irq.c g45_hpd_peg_band_gap_wa()
    fn g45_hpd_peg_band_gap_wa(&self, io: &mut impl HotplugIrqIo) {
        io.rmw(
            IrqRegister::PegBandGapData,
            PEG_BAND_GAP_DATA_MASK,
            PEG_BAND_GAP_DATA_VALUE,
        );
    }

    // upstream: intel_hotplug_irq.c i915_hpd_enable_detection()
    fn i915_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder: &Encoder) {
        let pin = encoder.hpd_pin as usize;
        let hotplug_en = hpd_i915_mask().get(pin).copied().unwrap_or(0);
        if self.platform.g45 {
            self.g45_hpd_peg_band_gap_wa(io);
        }
        self.i915_hotplug_interrupt_update(io, hotplug_en, hotplug_en);
    }

    // upstream: intel_hotplug_irq.c i915_hpd_irq_setup()
    fn i915_hpd_irq_setup(&self, io: &mut impl HotplugIrqIo) {
        io.assert_lock_held(IrqLock::Irq);
        let hotplug_en = self.intel_hpd_enabled_irqs(&hpd_i915_mask());
        let mut hotplug_en = hotplug_en;
        if self.platform.g4x {
            hotplug_en |= CRT_HOTPLUG_ACTIVATION_PERIOD_64;
        }
        hotplug_en |= CRT_HOTPLUG_VOLTAGE_COMPARE_50;
        if self.platform.g45 {
            self.g45_hpd_peg_band_gap_wa(io);
        }
        self.i915_hotplug_interrupt_update_locked(
            io,
            HOTPLUG_INT_EN_MASK
                | CRT_HOTPLUG_VOLTAGE_COMPARE_MASK
                | CRT_HOTPLUG_ACTIVATION_PERIOD_64,
            hotplug_en,
        );
    }

    // upstream: intel_hotplug_irq.c intel_hpd_enable_detection()
    pub fn intel_hpd_enable_detection(&self, io: &mut impl HotplugIrqIo, encoder_index: usize) {
        let Some(encoder) = self.encoders.get(encoder_index).copied() else {
            return;
        };
        match self.funcs {
            Some(HotplugFuncs::I915) => self.i915_hpd_enable_detection(io, &encoder),
            Some(HotplugFuncs::XelPdp) => self.xelpdp_hpd_enable_detection(io, &encoder),
            Some(HotplugFuncs::Dg1) => self.dg1_hpd_enable_detection(io, &encoder),
            Some(HotplugFuncs::Gen11) => self.gen11_hpd_enable_detection(io, &encoder),
            Some(HotplugFuncs::Bxt) => self.bxt_hpd_enable_detection(io, &encoder),
            Some(HotplugFuncs::Icp) => self.icp_hpd_enable_detection(io, &encoder),
            Some(HotplugFuncs::Spt) => self.spt_hpd_enable_detection(io, &encoder),
            Some(HotplugFuncs::Ilk) => self.ilk_hpd_enable_detection(io, &encoder),
            None => {}
        }
    }

    // upstream: intel_hotplug_irq.c intel_hpd_irq_setup()
    pub fn intel_hpd_irq_setup(&self, io: &mut impl HotplugIrqIo) {
        if (self.platform.valleyview || self.platform.cherryview)
            && !self.platform.vlv_display_irqs_enabled
        {
            return;
        }
        match self.funcs {
            Some(HotplugFuncs::I915) => self.i915_hpd_irq_setup(io),
            Some(HotplugFuncs::XelPdp) => self.xelpdp_hpd_irq_setup(io),
            Some(HotplugFuncs::Dg1) => self.dg1_hpd_irq_setup(io),
            Some(HotplugFuncs::Gen11) => self.gen11_hpd_irq_setup(io),
            Some(HotplugFuncs::Bxt) => self.bxt_hpd_irq_setup(io),
            Some(HotplugFuncs::Icp) => self.icp_hpd_irq_setup(io),
            Some(HotplugFuncs::Spt) => self.spt_hpd_irq_setup(io),
            Some(HotplugFuncs::Ilk) => self.ilk_hpd_irq_setup(io),
            None => {}
        }
    }

    // upstream: intel_hotplug_irq.c intel_hotplug_irq_init()
    pub fn intel_hotplug_irq_init(&mut self, io: &mut impl HotplugIrqIo) {
        self.intel_hpd_init_pins(io);
        io.hpd_init_early();
        let p = self.platform;
        self.funcs = if p.gmch {
            if p.has_hotplug {
                Some(HotplugFuncs::I915)
            } else {
                None
            }
        } else if p.pch == PchType::Dg2 {
            Some(HotplugFuncs::Icp)
        } else if p.pch == PchType::Dg1 {
            Some(HotplugFuncs::Dg1)
        } else if p.display_ver >= 14 {
            Some(HotplugFuncs::XelPdp)
        } else if p.display_ver >= 11 {
            Some(HotplugFuncs::Gen11)
        } else if p.geminilake || p.broxton {
            Some(HotplugFuncs::Bxt)
        } else if p.pch.at_least(PchType::Icp) {
            Some(HotplugFuncs::Icp)
        } else if p.pch.at_least(PchType::Spt) {
            Some(HotplugFuncs::Spt)
        } else {
            Some(HotplugFuncs::Ilk)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exported_gen11_map_preserves_tc_and_tbt_pin_bits() {
        let map = intel_hpd_gen11_pin_map();
        assert_eq!(map[HPD_PORT_TC1 as usize], (1 << 16) | 1);
        assert_eq!(map[HPD_PORT_TC2 as usize], (1 << 17) | 2);
        assert_eq!(map[HPD_PORT_A as usize], 0);
    }
}

const ZERO_HPD: [u32; HPD_NUM_PINS] = [0; HPD_NUM_PINS];

const fn hpd_ilk() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    h[HPD_PORT_A as usize] = bit(19);
    h
}
const fn hpd_ivb() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    h[HPD_PORT_A as usize] = bit(27);
    h
}
const fn hpd_bdw() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    h[HPD_PORT_A as usize] = gen8_de_port_hotplug(HPD_PORT_A);
    h
}
const fn hpd_ibx() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    h[HPD_CRT as usize] = bit(11);
    h[HPD_SDVO_B as usize] = bit(6);
    h[HPD_PORT_B as usize] = bit(8);
    h[HPD_PORT_C as usize] = bit(9);
    h[HPD_PORT_D as usize] = bit(10);
    h
}
const fn hpd_cpt() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    h[HPD_CRT as usize] = bit(19);
    h[HPD_SDVO_B as usize] = bit(18);
    h[HPD_PORT_B as usize] = bit(21);
    h[HPD_PORT_C as usize] = bit(22);
    h[HPD_PORT_D as usize] = bit(23);
    h
}
const fn hpd_spt() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    h[HPD_PORT_A as usize] = bit(24);
    h[HPD_PORT_B as usize] = bit(21);
    h[HPD_PORT_C as usize] = bit(22);
    h[HPD_PORT_D as usize] = bit(23);
    h[HPD_PORT_E as usize] = bit(25);
    h
}
const fn hpd_i915_mask() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    h[HPD_CRT as usize] = CRT_HOTPLUG_INT_EN;
    h[HPD_SDVO_B as usize] = SDVOB_HOTPLUG_INT_EN;
    h[HPD_SDVO_C as usize] = SDVOC_HOTPLUG_INT_EN;
    h[HPD_PORT_B as usize] = PORT_HOTPLUG_INT_EN_B;
    h[HPD_PORT_C as usize] = PORT_HOTPLUG_INT_EN_C;
    h[HPD_PORT_D as usize] = PORT_HOTPLUG_INT_EN_D;
    h
}
const fn hpd_g4x_status() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    h[HPD_CRT as usize] = CRT_HOTPLUG_INT_STATUS;
    h[HPD_SDVO_B as usize] = bit(2);
    h[HPD_SDVO_C as usize] = bit(3);
    h[HPD_PORT_B as usize] = PORTB_HOTPLUG_INT_STATUS;
    h[HPD_PORT_C as usize] = PORTC_HOTPLUG_INT_STATUS;
    h[HPD_PORT_D as usize] = PORTD_HOTPLUG_INT_STATUS;
    h
}
const fn hpd_i915_status() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    h[HPD_CRT as usize] = CRT_HOTPLUG_INT_STATUS;
    h[HPD_SDVO_B as usize] = bit(6);
    h[HPD_SDVO_C as usize] = bit(7);
    h[HPD_PORT_B as usize] = PORTB_HOTPLUG_INT_STATUS;
    h[HPD_PORT_C as usize] = PORTC_HOTPLUG_INT_STATUS;
    h[HPD_PORT_D as usize] = PORTD_HOTPLUG_INT_STATUS;
    h
}
const fn hpd_bxt() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    h[HPD_PORT_A as usize] = gen8_de_port_hotplug(HPD_PORT_A);
    h[HPD_PORT_B as usize] = gen8_de_port_hotplug(HPD_PORT_B);
    h[HPD_PORT_C as usize] = gen8_de_port_hotplug(HPD_PORT_C);
    h
}
const fn hpd_gen11() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    let mut pin = HPD_PORT_TC1;
    while pin <= HPD_PORT_TC6 {
        h[pin as usize] = gen11_tc_hotplug(pin) | gen11_tbt_hotplug(pin);
        pin += 1;
    }
    h
}

/// Return the source-selected Gen11+ hotplug mapping for kernel IRQ adapters.
/// The table remains generated by the same pin-bit helpers as i915 setup and
/// IRQ decode; exposing it avoids duplicating TC1/TC2 bit arithmetic in the
/// platform interrupt owner.
pub const fn intel_hpd_gen11_pin_map() -> [u32; HPD_NUM_PINS] {
    hpd_gen11()
}

const fn hpd_xelpdp() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    let mut pin = HPD_PORT_TC1;
    while pin <= HPD_PORT_TC4 {
        h[pin as usize] = xelpdp_tbt_hotplug(pin) | xelpdp_dp_alt_hotplug(pin);
        pin += 1;
    }
    h
}
const fn hpd_icp() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    let mut pin = HPD_PORT_A;
    while pin <= HPD_PORT_D {
        h[pin as usize] = sde_ddi_hotplug_icp(pin);
        pin += 1;
    }
    pin = HPD_PORT_TC1;
    while pin <= HPD_PORT_TC6 {
        h[pin as usize] = sde_tc_hotplug_icp(pin);
        pin += 1;
    }
    h
}
const fn hpd_sde_dg1() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    let mut pin = HPD_PORT_A;
    while pin <= HPD_PORT_D {
        h[pin as usize] = sde_ddi_hotplug_icp(pin);
        pin += 1;
    }
    h[HPD_PORT_TC1 as usize] = sde_tc_hotplug_dg2(HPD_PORT_TC1);
    h
}
const fn hpd_mtp() -> [u32; HPD_NUM_PINS] {
    let mut h = ZERO_HPD;
    h[HPD_PORT_A as usize] = sde_ddi_hotplug_icp(HPD_PORT_A);
    h[HPD_PORT_B as usize] = sde_ddi_hotplug_icp(HPD_PORT_B);
    let mut pin = HPD_PORT_TC1;
    while pin <= HPD_PORT_TC4 {
        h[pin as usize] = sde_tc_hotplug_icp(pin);
        pin += 1;
    }
    h
}
