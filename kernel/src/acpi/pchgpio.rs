//! Translated from OpenBSD sys/dev/acpi/pchgpio.c rev 1.19 (ISC).
//! Copyright (c) 2020 Mark Kettenis.
//! Copyright (c) 2020 James Hastings.
//! See LICENSES/ISC.txt and docs/upstream-provenance.md.
//!
//! Intel PCH GPIO community/pad layout and register operations.
#[cfg(target_os = "none")]
use alloc::{format, string::String, vec::Vec};
use core::ptr::{read_volatile, write_volatile};

#[cfg(target_os = "none")]
use kspin::SpinNoIrq;
#[cfg(target_os = "none")]
use tk_acpica::{Engine, Node};

const CONF_TXSTATE: u32 = 0x0000_0001;
const CONF_RXSTATE: u32 = 0x0000_0002;
const CONF_RXINV: u32 = 0x0080_0000;
const CONF_RXEV_EDGE: u32 = 0x0200_0000;
const CONF_RXEV_ZERO: u32 = 0x0400_0000;
const CONF_RXEV_MASK: u32 = 0x0600_0000;
const CONF_PADRSTCFG_MASK: u32 = 0xc000_0000;
const PADBAR: usize = 0x00c;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Group {
    pub bar: u8,
    pub bank: u8,
    pub base: u16,
    pub limit: u16,
    pub gpiobase: i16,
}
#[derive(Debug)]
pub struct Device {
    pub pad_size: u16,
    pub gpi_is: u16,
    pub gpi_ie: u16,
    pub groups: &'static [Group],
    pub npins: usize,
}

// upstream: pchgpio.c spt_lp_groups
pub static SPT_LP_GROUPS: [Group; 7] = [
    Group {
        bar: 0,
        bank: 0,
        base: 0,
        limit: 23,
        gpiobase: 0,
    },
    Group {
        bar: 0,
        bank: 1,
        base: 24,
        limit: 47,
        gpiobase: 24,
    },
    Group {
        bar: 1,
        bank: 0,
        base: 48,
        limit: 71,
        gpiobase: 48,
    },
    Group {
        bar: 1,
        bank: 1,
        base: 72,
        limit: 95,
        gpiobase: 72,
    },
    Group {
        bar: 1,
        bank: 2,
        base: 96,
        limit: 119,
        gpiobase: 96,
    },
    Group {
        bar: 2,
        bank: 0,
        base: 120,
        limit: 143,
        gpiobase: 120,
    },
    Group {
        bar: 2,
        bank: 1,
        base: 144,
        limit: 151,
        gpiobase: 144,
    },
];

// upstream: pchgpio.c spt_h_groups
pub static SPT_H_GROUPS: [Group; 9] = [
    Group {
        bar: 0,
        bank: 0,
        base: 0,
        limit: 23,
        gpiobase: 0,
    },
    Group {
        bar: 0,
        bank: 1,
        base: 24,
        limit: 47,
        gpiobase: 24,
    },
    Group {
        bar: 1,
        bank: 0,
        base: 48,
        limit: 71,
        gpiobase: 48,
    },
    Group {
        bar: 1,
        bank: 1,
        base: 72,
        limit: 95,
        gpiobase: 72,
    },
    Group {
        bar: 1,
        bank: 2,
        base: 96,
        limit: 108,
        gpiobase: 96,
    },
    Group {
        bar: 1,
        bank: 3,
        base: 109,
        limit: 132,
        gpiobase: 120,
    },
    Group {
        bar: 1,
        bank: 4,
        base: 133,
        limit: 156,
        gpiobase: 144,
    },
    Group {
        bar: 1,
        bank: 5,
        base: 157,
        limit: 180,
        gpiobase: 168,
    },
    Group {
        bar: 2,
        bank: 0,
        base: 181,
        limit: 191,
        gpiobase: 192,
    },
];

// upstream: pchgpio.c cnl_h_groups
pub static CNL_H_GROUPS: [Group; 11] = [
    Group {
        bar: 0,
        bank: 0,
        base: 0,
        limit: 24,
        gpiobase: 0,
    },
    Group {
        bar: 0,
        bank: 1,
        base: 25,
        limit: 50,
        gpiobase: 32,
    },
    Group {
        bar: 1,
        bank: 0,
        base: 51,
        limit: 74,
        gpiobase: 64,
    },
    Group {
        bar: 1,
        bank: 1,
        base: 75,
        limit: 98,
        gpiobase: 96,
    },
    Group {
        bar: 1,
        bank: 2,
        base: 99,
        limit: 106,
        gpiobase: 128,
    },
    Group {
        bar: 2,
        bank: 0,
        base: 155,
        limit: 178,
        gpiobase: 192,
    },
    Group {
        bar: 2,
        bank: 1,
        base: 179,
        limit: 202,
        gpiobase: 224,
    },
    Group {
        bar: 2,
        bank: 2,
        base: 203,
        limit: 215,
        gpiobase: 256,
    },
    Group {
        bar: 2,
        bank: 3,
        base: 216,
        limit: 239,
        gpiobase: 288,
    },
    Group {
        bar: 3,
        bank: 2,
        base: 269,
        limit: 286,
        gpiobase: 320,
    },
    Group {
        bar: 3,
        bank: 3,
        base: 287,
        limit: 298,
        gpiobase: 352,
    },
];

// upstream: pchgpio.c cnl_lp_groups
pub static CNL_LP_GROUPS: [Group; 8] = [
    Group {
        bar: 0,
        bank: 0,
        base: 0,
        limit: 24,
        gpiobase: 0,
    },
    Group {
        bar: 0,
        bank: 1,
        base: 25,
        limit: 50,
        gpiobase: 32,
    },
    Group {
        bar: 0,
        bank: 2,
        base: 51,
        limit: 58,
        gpiobase: 64,
    },
    Group {
        bar: 1,
        bank: 0,
        base: 68,
        limit: 92,
        gpiobase: 96,
    },
    Group {
        bar: 1,
        bank: 1,
        base: 93,
        limit: 116,
        gpiobase: 128,
    },
    Group {
        bar: 1,
        bank: 2,
        base: 117,
        limit: 140,
        gpiobase: 160,
    },
    Group {
        bar: 2,
        bank: 0,
        base: 181,
        limit: 204,
        gpiobase: 256,
    },
    Group {
        bar: 2,
        bank: 1,
        base: 205,
        limit: 228,
        gpiobase: 288,
    },
];

// upstream: pchgpio.c tgl_lp_groups
pub static TGL_LP_GROUPS: [Group; 11] = [
    Group {
        bar: 0,
        bank: 0,
        base: 0,
        limit: 25,
        gpiobase: 0,
    },
    Group {
        bar: 0,
        bank: 1,
        base: 26,
        limit: 41,
        gpiobase: 32,
    },
    Group {
        bar: 0,
        bank: 2,
        base: 42,
        limit: 66,
        gpiobase: 64,
    },
    Group {
        bar: 1,
        bank: 0,
        base: 67,
        limit: 74,
        gpiobase: 96,
    },
    Group {
        bar: 1,
        bank: 1,
        base: 75,
        limit: 98,
        gpiobase: 128,
    },
    Group {
        bar: 1,
        bank: 2,
        base: 99,
        limit: 119,
        gpiobase: 160,
    },
    Group {
        bar: 1,
        bank: 3,
        base: 120,
        limit: 143,
        gpiobase: 192,
    },
    Group {
        bar: 2,
        bank: 0,
        base: 171,
        limit: 194,
        gpiobase: 256,
    },
    Group {
        bar: 2,
        bank: 1,
        base: 195,
        limit: 219,
        gpiobase: 288,
    },
    Group {
        bar: 2,
        bank: 3,
        base: 226,
        limit: 250,
        gpiobase: 320,
    },
    Group {
        bar: 3,
        bank: 0,
        base: 260,
        limit: 267,
        gpiobase: 352,
    },
];

// upstream: pchgpio.c tgl_h_groups
pub static TGL_H_GROUPS: [Group; 13] = [
    Group {
        bar: 0,
        bank: 0,
        base: 0,
        limit: 24,
        gpiobase: 0,
    },
    Group {
        bar: 0,
        bank: 1,
        base: 25,
        limit: 44,
        gpiobase: 32,
    },
    Group {
        bar: 0,
        bank: 2,
        base: 45,
        limit: 70,
        gpiobase: 64,
    },
    Group {
        bar: 1,
        bank: 0,
        base: 79,
        limit: 104,
        gpiobase: 128,
    },
    Group {
        bar: 1,
        bank: 1,
        base: 105,
        limit: 128,
        gpiobase: 160,
    },
    Group {
        bar: 1,
        bank: 2,
        base: 129,
        limit: 136,
        gpiobase: 192,
    },
    Group {
        bar: 1,
        bank: 3,
        base: 137,
        limit: 153,
        gpiobase: 224,
    },
    Group {
        bar: 2,
        bank: 0,
        base: 181,
        limit: 193,
        gpiobase: 288,
    },
    Group {
        bar: 2,
        bank: 1,
        base: 194,
        limit: 217,
        gpiobase: 320,
    },
    Group {
        bar: 2,
        bank: 0,
        base: 218,
        limit: 241,
        gpiobase: 352,
    },
    Group {
        bar: 2,
        bank: 1,
        base: 242,
        limit: 251,
        gpiobase: 384,
    },
    Group {
        bar: 2,
        bank: 2,
        base: 252,
        limit: 266,
        gpiobase: 416,
    },
    Group {
        bar: 3,
        bank: 0,
        base: 267,
        limit: 281,
        gpiobase: 448,
    },
];

// upstream: pchgpio.c adl_s_groups
pub static ADL_S_GROUPS: [Group; 13] = [
    Group {
        bar: 0,
        bank: 0,
        base: 0,
        limit: 24,
        gpiobase: 0,
    },
    Group {
        bar: 0,
        bank: 1,
        base: 25,
        limit: 47,
        gpiobase: 32,
    },
    Group {
        bar: 0,
        bank: 2,
        base: 48,
        limit: 59,
        gpiobase: 64,
    },
    Group {
        bar: 1,
        bank: 0,
        base: 95,
        limit: 118,
        gpiobase: 160,
    },
    Group {
        bar: 1,
        bank: 1,
        base: 119,
        limit: 126,
        gpiobase: 192,
    },
    Group {
        bar: 1,
        bank: 2,
        base: 127,
        limit: 150,
        gpiobase: 224,
    },
    Group {
        bar: 2,
        bank: 1,
        base: 160,
        limit: 175,
        gpiobase: 256,
    },
    Group {
        bar: 2,
        bank: 2,
        base: 176,
        limit: 199,
        gpiobase: 288,
    },
    Group {
        bar: 3,
        bank: 0,
        base: 200,
        limit: 207,
        gpiobase: 320,
    },
    Group {
        bar: 3,
        bank: 1,
        base: 208,
        limit: 230,
        gpiobase: 352,
    },
    Group {
        bar: 3,
        bank: 2,
        base: 231,
        limit: 245,
        gpiobase: 384,
    },
    Group {
        bar: 3,
        bank: 3,
        base: 246,
        limit: 269,
        gpiobase: 416,
    },
    Group {
        bar: 4,
        bank: 0,
        base: 270,
        limit: 294,
        gpiobase: 448,
    },
];

// upstream: pchgpio.c adl_n_groups
pub static ADL_N_GROUPS: [Group; 11] = [
    Group {
        bar: 0,
        bank: 0,
        base: 0,
        limit: 25,
        gpiobase: 0,
    },
    Group {
        bar: 0,
        bank: 1,
        base: 26,
        limit: 41,
        gpiobase: 32,
    },
    Group {
        bar: 0,
        bank: 2,
        base: 42,
        limit: 66,
        gpiobase: 64,
    },
    Group {
        bar: 1,
        bank: 0,
        base: 67,
        limit: 74,
        gpiobase: 96,
    },
    Group {
        bar: 1,
        bank: 1,
        base: 75,
        limit: 94,
        gpiobase: 128,
    },
    Group {
        bar: 1,
        bank: 2,
        base: 95,
        limit: 118,
        gpiobase: 160,
    },
    Group {
        bar: 1,
        bank: 3,
        base: 119,
        limit: 139,
        gpiobase: 192,
    },
    Group {
        bar: 2,
        bank: 0,
        base: 169,
        limit: 192,
        gpiobase: 256,
    },
    Group {
        bar: 2,
        bank: 1,
        base: 193,
        limit: 217,
        gpiobase: 288,
    },
    Group {
        bar: 2,
        bank: 3,
        base: 224,
        limit: 248,
        gpiobase: 320,
    },
    Group {
        bar: 3,
        bank: 0,
        base: 249,
        limit: 256,
        gpiobase: 352,
    },
];

// upstream: pchgpio.c mtl_p_groups
pub static MTL_P_GROUPS: [Group; 9] = [
    Group {
        bar: 0,
        bank: 1,
        base: 5,
        limit: 28,
        gpiobase: 32,
    },
    Group {
        bar: 0,
        bank: 2,
        base: 29,
        limit: 52,
        gpiobase: 64,
    },
    Group {
        bar: 1,
        bank: 0,
        base: 53,
        limit: 77,
        gpiobase: 96,
    },
    Group {
        bar: 1,
        bank: 1,
        base: 78,
        limit: 102,
        gpiobase: 128,
    },
    Group {
        bar: 2,
        bank: 0,
        base: 103,
        limit: 128,
        gpiobase: 160,
    },
    Group {
        bar: 2,
        bank: 1,
        base: 129,
        limit: 154,
        gpiobase: 192,
    },
    Group {
        bar: 3,
        bank: 0,
        base: 184,
        limit: 191,
        gpiobase: 288,
    },
    Group {
        bar: 4,
        bank: 0,
        base: 204,
        limit: 228,
        gpiobase: 352,
    },
    Group {
        bar: 4,
        bank: 1,
        base: 229,
        limit: 253,
        gpiobase: 384,
    },
];

// upstream: pchgpio.c mtl_s_groups
pub static MTL_S_GROUPS: [Group; 4] = [
    Group {
        bar: 0,
        bank: 0,
        base: 0,
        limit: 27,
        gpiobase: 0,
    },
    Group {
        bar: 0,
        bank: 2,
        base: 47,
        limit: 73,
        gpiobase: 64,
    },
    Group {
        bar: 1,
        bank: 0,
        base: 74,
        limit: 93,
        gpiobase: 96,
    },
    Group {
        bar: 1,
        bank: 2,
        base: 96,
        limit: 119,
        gpiobase: 160,
    },
];
pub static SPT_LP_DEVICE: Device = Device {
    pad_size: 8,
    gpi_is: 0x100,
    gpi_ie: 0x120,
    groups: &SPT_LP_GROUPS,
    npins: 176,
};
pub static SPT_H_DEVICE: Device = Device {
    pad_size: 8,
    gpi_is: 0x100,
    gpi_ie: 0x120,
    groups: &SPT_H_GROUPS,
    npins: 224,
};
pub static CNL_H_DEVICE: Device = Device {
    pad_size: 16,
    gpi_is: 0x100,
    gpi_ie: 0x120,
    groups: &CNL_H_GROUPS,
    npins: 384,
};
pub static CNL_LP_DEVICE: Device = Device {
    pad_size: 16,
    gpi_is: 0x100,
    gpi_ie: 0x120,
    groups: &CNL_LP_GROUPS,
    npins: 320,
};
pub static TGL_LP_DEVICE: Device = Device {
    pad_size: 16,
    gpi_is: 0x100,
    gpi_ie: 0x120,
    groups: &TGL_LP_GROUPS,
    npins: 360,
};
pub static TGL_H_DEVICE: Device = Device {
    pad_size: 16,
    gpi_is: 0x100,
    gpi_ie: 0x120,
    groups: &TGL_H_GROUPS,
    npins: 480,
};
pub static ADL_S_DEVICE: Device = Device {
    pad_size: 16,
    gpi_is: 0x200,
    gpi_ie: 0x220,
    groups: &ADL_S_GROUPS,
    npins: 480,
};
pub static ADL_N_DEVICE: Device = Device {
    pad_size: 16,
    gpi_is: 0x100,
    gpi_ie: 0x120,
    groups: &ADL_N_GROUPS,
    npins: 384,
};
pub static MTL_P_DEVICE: Device = Device {
    pad_size: 16,
    gpi_is: 0x200,
    gpi_ie: 0x210,
    groups: &MTL_P_GROUPS,
    npins: 416,
};
pub static MTL_S_DEVICE: Device = Device {
    pad_size: 16,
    gpi_is: 0x200,
    gpi_ie: 0x210,
    groups: &MTL_S_GROUPS,
    npins: 192,
};

pub const HIDS: &[(&str, &Device)] = &[
    ("INT344B", &SPT_LP_DEVICE),
    ("INT3450", &CNL_H_DEVICE),
    ("INT3451", &SPT_H_DEVICE),
    ("INT345D", &SPT_H_DEVICE),
    ("INT34BB", &CNL_LP_DEVICE),
    ("INT34C5", &TGL_LP_DEVICE),
    ("INT34C6", &TGL_H_DEVICE),
    ("INTC1055", &TGL_LP_DEVICE),
    ("INTC1056", &ADL_S_DEVICE),
    ("INTC1057", &ADL_N_DEVICE),
    ("INTC1085", &ADL_S_DEVICE),
    ("INTC1082", &MTL_S_DEVICE),
    ("INTC1083", &MTL_P_DEVICE),
    ("INTC105E", &MTL_P_DEVICE),
];

pub fn device_for_hid(hid: &str) -> Option<&'static Device> {
    HIDS.iter()
        .find(|(name, _)| *name == hid)
        .map(|(_, device)| *device)
}

// upstream: pchgpio.c pchgpio_find_group()
pub fn find_group(device: &Device, pin: usize) -> Option<(usize, &Group)> {
    device.groups.iter().enumerate().find(|(_, group)| {
        let count = 1usize + usize::from(group.limit - group.base);
        pin >= group.gpiobase as usize && pin < (group.gpiobase as usize).saturating_add(count)
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PinConfig {
    pub pad_cfg_dw0: u32,
    pub pad_cfg_dw1: u32,
    pub gpi_ie: bool,
}

#[derive(Clone)]
struct PinInterrupt {
    pending: alloc::sync::Arc<core::sync::atomic::AtomicBool>,
    level: u8,
}

pub struct Controller {
    pub device: &'static Device,
    pub bars: [usize; 5],
    pub padbar: [u16; 5],
    pub padbase: [u16; 5],
    pin_cfg: alloc::vec::Vec<PinConfig>,
    pin_interrupts: alloc::vec::Vec<Option<PinInterrupt>>,
}
impl Controller {
    // upstream: pchgpio.c pchgpio_attach()
    pub unsafe fn new(device: &'static Device, bars: [usize; 5]) -> Option<Self> {
        let mut pin_cfg = alloc::vec::Vec::new();
        pin_cfg.try_reserve_exact(device.npins).ok()?;
        pin_cfg.resize(device.npins, PinConfig::default());
        let mut pin_interrupts = alloc::vec::Vec::new();
        pin_interrupts.try_reserve_exact(device.npins).ok()?;
        pin_interrupts.resize(device.npins, None);
        let mut this = Self {
            device,
            bars,
            padbar: [0; 5],
            padbase: [0; 5],
            pin_cfg,
            pin_interrupts,
        };
        for (bar, address) in bars.iter().copied().enumerate() {
            if address != 0 {
                this.padbar[bar] =
                    // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
                    unsafe { read_volatile((address + PADBAR) as *const u32) as u16 };
            }
        }
        let mut seen = [false; 5];
        for group in device.groups {
            let bar = usize::from(group.bar);
            if !seen[bar] {
                this.padbase[bar] = group.base;
                seen[bar] = true;
            }
        }
        Some(this)
    }
    fn pad(&self, pin: usize) -> Option<(usize, usize, usize, usize)> {
        let (_, group) = find_group(self.device, pin)?;
        let bar = usize::from(group.bar);
        let pad =
            usize::from(group.base + (pin - group.gpiobase as usize) as u16 - self.padbase[bar]);
        Some((
            bar,
            pad,
            usize::from(group.bank),
            pin - group.gpiobase as usize,
        ))
    }
    // upstream: pchgpio.c pchgpio_read_pin()
    pub fn read_pin(&self, pin: usize) -> Option<bool> {
        let (bar, pad, ..) = self.pad(pin)?;
        let address = self.bars[bar]
            .checked_add(usize::from(self.padbar[bar]))?
            .checked_add(pad * usize::from(self.device.pad_size))?;
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        Some(unsafe { read_volatile(address as *const u32) } & CONF_RXSTATE != 0)
    }
    // upstream: pchgpio.c pchgpio_write_pin()
    pub fn write_pin(&self, pin: usize, value: bool) -> bool {
        let Some((bar, pad, ..)) = self.pad(pin) else {
            return false;
        };
        let Some(address) = self.bars[bar]
            .checked_add(usize::from(self.padbar[bar]))
            .and_then(|v| v.checked_add(pad * usize::from(self.device.pad_size)))
        else {
            return false;
        };
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        let mut reg = unsafe { read_volatile(address as *const u32) };
        if value {
            reg |= CONF_TXSTATE
        } else {
            reg &= !CONF_TXSTATE
        };
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        unsafe { write_volatile(address as *mut u32, reg) };
        true
    }
    // upstream: pchgpio.c pchgpio_intr_establish(), pchgpio_intr_enable(), pchgpio_intr_disable()
    pub fn configure_interrupt(
        &self,
        pin: usize,
        edge: bool,
        active_low: bool,
        both: bool,
        enabled: bool,
    ) -> bool {
        let Some((bar, pad, bank, bit)) = self.pad(pin) else {
            return false;
        };
        let Some(pad_addr) = self.bars[bar]
            .checked_add(usize::from(self.padbar[bar]))
            .and_then(|v| v.checked_add(pad * usize::from(self.device.pad_size)))
        else {
            return false;
        };
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        let mut cfg = unsafe { read_volatile(pad_addr as *const u32) };
        cfg &= !(CONF_RXEV_MASK | CONF_RXINV);
        if edge {
            cfg |= CONF_RXEV_EDGE;
        }
        if active_low {
            cfg |= CONF_RXINV;
        }
        if both {
            cfg |= CONF_RXEV_EDGE | CONF_RXEV_ZERO;
        }
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        unsafe { write_volatile(pad_addr as *mut u32, cfg) };
        let enable_addr = self.bars[bar]
            .checked_add(usize::from(self.device.gpi_ie))
            .and_then(|v| v.checked_add(bank * 4));
        let Some(enable_addr) = enable_addr else {
            return false;
        };
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        let mut enable = unsafe { read_volatile(enable_addr as *const u32) };
        if enabled {
            enable |= 1u32 << bit
        } else {
            enable &= !(1u32 << bit)
        };
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        unsafe { write_volatile(enable_addr as *mut u32, enable) };
        true
    }
    // upstream: pchgpio.c pchgpio_intr_establish()
    pub fn establish_interrupt(
        &mut self,
        pin: usize,
        edge: bool,
        active_low: bool,
        both: bool,
        level: u8,
        pending: alloc::sync::Arc<core::sync::atomic::AtomicBool>,
    ) -> bool {
        if pin >= self.device.npins
            || self.pin_interrupts[pin].is_some()
            || !self.configure_interrupt(pin, edge, active_low, both, false)
        {
            return false;
        }
        self.pin_interrupts[pin] = Some(PinInterrupt {
            pending,
            level: level & !0x80, // IPL_WAKEUP is not an execution level.
        });
        true
    }
    // upstream: pchgpio.c pchgpio_intr_enable()
    pub fn enable_interrupt(&self, pin: usize) -> bool {
        let Some((bar, pad, bank, bit)) = self.pad(pin) else {
            return false;
        };
        let _ = (bar, pad);
        let Some(address) = self.bars[bar]
            .checked_add(usize::from(self.device.gpi_ie))
            .and_then(|v| v.checked_add(bank * 4))
        else {
            return false;
        };
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        let mut enable = unsafe { read_volatile(address as *const u32) };
        enable |= 1u32 << bit;
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        unsafe { write_volatile(address as *mut u32, enable) };
        true
    }
    // upstream: pchgpio.c pchgpio_intr_disable()
    pub fn disable_interrupt(&self, pin: usize) -> bool {
        let Some((bar, pad, bank, bit)) = self.pad(pin) else {
            return false;
        };
        let _ = (pad,);
        let Some(address) = self.bars[bar]
            .checked_add(usize::from(self.device.gpi_ie))
            .and_then(|v| v.checked_add(bank * 4))
        else {
            return false;
        };
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        let mut enable = unsafe { read_volatile(address as *const u32) };
        enable &= !(1u32 << bit);
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        unsafe { write_volatile(address as *mut u32, enable) };
        true
    }
    pub fn release_interrupt(&mut self, pin: usize) -> bool {
        if pin >= self.pin_interrupts.len() || !self.disable_interrupt(pin) {
            return false;
        }
        self.pin_interrupts[pin] = None;
        true
    }
    // upstream: pchgpio.c pchgpio_intr_handle()
    fn dispatch_pin(&self, pin: usize) -> bool {
        if let Some(handler) = self.pin_interrupts.get(pin).cloned().flatten() {
            let _ipl = handler.level;
            let _ = handler.level;
            handler
                .pending
                .store(true, core::sync::atomic::Ordering::Release);
            true
        } else {
            let _ = self.disable_interrupt(pin);
            false
        }
    }
    // upstream: pchgpio.c pchgpio_intr()
    pub fn dispatch_interrupt(&self) -> bool {
        let mut handled = false;
        for group in self.device.groups {
            let bar = usize::from(group.bar);
            let bank = usize::from(group.bank);
            let Some(status_address) = self.bars[bar]
                .checked_add(usize::from(self.device.gpi_is))
                .and_then(|v| v.checked_add(bank * 4))
            else {
                continue;
            };
            // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
            let status = unsafe { read_volatile(status_address as *const u32) };
            if status == 0 {
                continue;
            }
            // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
            unsafe { write_volatile(status_address as *mut u32, status) };
            let Some(enable_address) = self.bars[bar]
                .checked_add(usize::from(self.device.gpi_ie))
                .and_then(|v| v.checked_add(bank * 4))
            else {
                continue;
            };
            // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
            let enabled = unsafe { read_volatile(enable_address as *const u32) };
            let active = status & enabled;
            if active == 0 {
                continue;
            }
            let count = usize::from(group.limit - group.base) + 1;
            for bit in 0..count.min(32) {
                if active & (1 << bit) != 0 {
                    let pin = group.gpiobase as usize + bit;
                    handled |= self.dispatch_pin(pin);
                }
            }
        }
        handled
    }
    // upstream: pchgpio.c pchgpio_save_pin()
    pub fn save_pin(&self, pin: usize, saved: &mut PinConfig) -> bool {
        let Some((bar, pad, bank, bit)) = self.pad(pin) else {
            return false;
        };
        let Some(pad_addr) = self.bars[bar]
            .checked_add(usize::from(self.padbar[bar]))
            .and_then(|v| v.checked_add(pad * usize::from(self.device.pad_size)))
        else {
            return false;
        };
        let Some(enable_addr) = self.bars[bar]
            .checked_add(usize::from(self.device.gpi_ie))
            .and_then(|v| v.checked_add(bank * 4))
        else {
            return false;
        };
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        saved.pad_cfg_dw0 = unsafe { read_volatile(pad_addr as *const u32) };
        // SAFETY: PAD_CFG_DW1 is the next 32-bit word in this checked MMIO pad window.
        saved.pad_cfg_dw1 = unsafe { read_volatile((pad_addr + 4) as *const u32) };
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        saved.gpi_ie = unsafe { read_volatile(enable_addr as *const u32) } & (1u32 << bit) != 0;
        true
    }
    // upstream: pchgpio.c pchgpio_restore_pin()
    pub fn restore_pin(&self, pin: usize, saved: &PinConfig, has_interrupt: bool) -> bool {
        let Some((bar, pad, bank, bit)) = self.pad(pin) else {
            return false;
        };
        let Some(pad_addr) = self.bars[bar]
            .checked_add(usize::from(self.padbar[bar]))
            .and_then(|v| v.checked_add(pad * usize::from(self.device.pad_size)))
        else {
            return false;
        };
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        let current = unsafe { read_volatile(pad_addr as *const u32) };
        let restore = has_interrupt
            || (saved.pad_cfg_dw0 & CONF_PADRSTCFG_MASK) != (current & CONF_PADRSTCFG_MASK);
        if restore {
            // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
            unsafe {
                write_volatile(pad_addr as *mut u32, saved.pad_cfg_dw0);
                write_volatile((pad_addr + 4) as *mut u32, saved.pad_cfg_dw1);
            }
            let Some(enable_addr) = self.bars[bar]
                .checked_add(usize::from(self.device.gpi_ie))
                .and_then(|v| v.checked_add(bank * 4))
            else {
                return false;
            };
            // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
            let mut enabled = unsafe { read_volatile(enable_addr as *const u32) };
            if saved.gpi_ie {
                enabled |= 1u32 << bit
            } else {
                enabled &= !(1u32 << bit)
            };
            // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
            unsafe { write_volatile(enable_addr as *mut u32, enabled) };
        }
        true
    }
    // upstream: pchgpio.c pchgpio_save()
    pub fn save(&mut self) {
        for pin in 0..self.device.npins {
            let mut saved = PinConfig::default();
            if self.save_pin(pin, &mut saved) {
                self.pin_cfg[pin] = saved;
            }
        }
    }
    // upstream: pchgpio.c pchgpio_restore()
    pub fn restore(&self) {
        for pin in 0..self.device.npins {
            let _ = self.restore_pin(pin, &self.pin_cfg[pin], self.pin_interrupts[pin].is_some());
        }
    }
    // upstream: pchgpio.c pchgpio_intr()
    pub fn pending(&self, bar: usize, bank: usize) -> Option<u32> {
        let address = self
            .bars
            .get(bar)?
            .checked_add(usize::from(self.device.gpi_is))?
            .checked_add(bank * 4)?;
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        Some(unsafe { read_volatile(address as *const u32) })
    }
    // upstream: pchgpio.c pchgpio_intr()
    pub fn acknowledge(&self, bar: usize, bank: usize, bits: u32) -> bool {
        let Some(address) = self
            .bars
            .get(bar)
            .and_then(|b| b.checked_add(usize::from(self.device.gpi_is)))
            .and_then(|v| v.checked_add(bank * 4))
        else {
            return false;
        };
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        unsafe { write_volatile(address as *mut u32, bits) };
        true
    }
}

#[cfg(target_os = "none")]
struct Provider {
    path: String,
    controller: Controller,
}

#[cfg(target_os = "none")]
static PROVIDERS: SpinNoIrq<Vec<Provider>> = SpinNoIrq::new(Vec::new());
#[cfg(target_os = "none")]
const MAX_PROVIDERS: usize = 8;

#[cfg(target_os = "none")]
fn dispatch_provider(slot: usize) {
    if let Some(provider) = PROVIDERS.lock().get(slot) {
        let _ = provider.controller.dispatch_interrupt();
    }
}
#[cfg(target_os = "none")]
fn gpio_irq_0() {
    dispatch_provider(0);
}
#[cfg(target_os = "none")]
fn gpio_irq_1() {
    dispatch_provider(1);
}
#[cfg(target_os = "none")]
fn gpio_irq_2() {
    dispatch_provider(2);
}
#[cfg(target_os = "none")]
fn gpio_irq_3() {
    dispatch_provider(3);
}
#[cfg(target_os = "none")]
fn gpio_irq_4() {
    dispatch_provider(4);
}
#[cfg(target_os = "none")]
fn gpio_irq_5() {
    dispatch_provider(5);
}
#[cfg(target_os = "none")]
fn gpio_irq_6() {
    dispatch_provider(6);
}
#[cfg(target_os = "none")]
fn gpio_irq_7() {
    dispatch_provider(7);
}
#[cfg(target_os = "none")]
fn gpio_irq(slot: usize) -> Option<fn()> {
    Some(match slot {
        0 => gpio_irq_0,
        1 => gpio_irq_1,
        2 => gpio_irq_2,
        3 => gpio_irq_3,
        4 => gpio_irq_4,
        5 => gpio_irq_5,
        6 => gpio_irq_6,
        7 => gpio_irq_7,
        _ => return None,
    })
}

#[cfg(target_os = "none")]
fn map_memory(base: u64, length: u64) -> Option<usize> {
    use axhal::mem::{PhysAddr, phys_to_virt};
    let base = usize::try_from(base).ok()?;
    let length = usize::try_from(length).ok()?;
    if base == 0
        || length < 0x1000
        || length > 16 * 1024 * 1024
        || base.checked_add(length).is_none()
    {
        return None;
    }
    let page = base & !0xfff;
    let end = base.checked_add(length)?.checked_add(0xfff)? & !0xfff;
    let mut current = page;
    while current < end {
        let address = phys_to_virt(PhysAddr::from_usize(current));
        let mapped = axmm::kernel_aspace().lock().query_leaf(address).is_ok();
        if !mapped && axmm::iomap(PhysAddr::from_usize(current), 4096).is_err() {
            return None;
        }
        current = current.checked_add(4096)?;
    }
    Some(phys_to_virt(PhysAddr::from_usize(base)).as_usize())
}

#[cfg(target_os = "none")]
fn attach_provider(engine: &Engine, node: &Node) -> bool {
    if engine
        .integer(&format!("{}._STA", node.path))
        .is_ok_and(|status| status & 1 == 0)
    {
        return false;
    }
    let Ok(hid) = engine.hardware_id(&node.path) else {
        return false;
    };
    let Some(device) = device_for_hid(&hid) else {
        return false;
    };
    let Ok(resources) = engine.resources(&node.path, false) else {
        return false;
    };
    let Ok(memory) = tk_acpica::resources::parse_memory_ranges(&resources) else {
        return false;
    };
    let Ok(interrupts) = tk_acpica::resources::parse(&resources) else {
        return false;
    };
    let Some(irq_resource) = interrupts.irqs.iter().find(|irq| !irq.numbers.is_empty()) else {
        return false;
    };
    let irq = irq_resource.numbers[0];
    let mut bars = [0usize; 5];
    for (index, range) in memory.iter().take(bars.len()).enumerate() {
        let Some(base) = map_memory(range.base, range.length) else {
            return false;
        };
        bars[index] = base;
    }
    if device
        .groups
        .iter()
        .any(|group| bars[usize::from(group.bar)] == 0)
    {
        return false;
    }
    // SAFETY: assigned _CRS memory descriptors were checked and mapped above.
    let Some(controller) = (unsafe { Controller::new(device, bars) }) else {
        return false;
    };
    let mut providers = PROVIDERS.lock();
    if providers.len() >= MAX_PROVIDERS || providers.try_reserve(1).is_err() {
        return false;
    }
    let slot = providers.len();
    let Some(handler) = gpio_irq(slot) else {
        return false;
    };
    providers.push(Provider {
        path: node.path.clone(),
        controller,
    });
    let Some(_vector) =
        axhal::acpi::install_gsi(irq, irq_resource.level, irq_resource.active_low, handler)
    else {
        providers.pop();
        return false;
    };
    info!(
        "pchgpio: attached {hid} path={} pins={} irq={irq}",
        node.path, device.npins
    );
    true
}

#[cfg(target_os = "none")]
pub fn init(engine: &Engine, nodes: &[Node]) -> usize {
    let mut attached = 0;
    for node in nodes.iter().filter(|node| node.kind == 6) {
        if attach_provider(engine, node) {
            attached += 1;
        }
    }
    attached
}

/// Snapshot every attached controller before the platform enters S3. This is
/// the GPIO half of the ACPI suspend lifecycle; the caller must invoke
/// [`restore_all`] after firmware resumes and before child drivers use pins.
#[cfg(target_os = "none")]
pub fn save_all() {
    for provider in PROVIDERS.lock().iter_mut() {
        provider.controller.save();
    }
}

/// Restore saved pad configuration and interrupt enables after S3 resume.
#[cfg(target_os = "none")]
pub fn restore_all() {
    for provider in PROVIDERS.lock().iter() {
        provider.controller.restore();
    }
}

#[cfg(not(target_os = "none"))]
pub fn init(_engine: &tk_acpica::Engine, _nodes: &[tk_acpica::Node]) -> usize {
    0
}

#[cfg(not(target_os = "none"))]
pub fn save_all() {}

#[cfg(not(target_os = "none"))]
pub fn restore_all() {}

struct GpioServices;
#[crate_interface::impl_interface]
impl axdriver::i2c::AcpiGpioSupport for GpioServices {
    fn request_interrupt(
        controller_path: &str,
        pin: u16,
        edge_triggered: bool,
        polarity: axdriver::i2c::AcpiGpioPolarity,
        level: u8,
        pending: alloc::sync::Arc<core::sync::atomic::AtomicBool>,
    ) -> Option<u64> {
        #[cfg(target_os = "none")]
        {
            let mut providers = PROVIDERS.lock();
            let (slot, provider) = providers
                .iter_mut()
                .enumerate()
                .find(|(_, provider)| provider.path == controller_path)?;
            let both = polarity == axdriver::i2c::AcpiGpioPolarity::Both;
            let active_low = polarity == axdriver::i2c::AcpiGpioPolarity::ActiveLow;
            if !provider.controller.establish_interrupt(
                usize::from(pin),
                edge_triggered,
                active_low,
                both,
                level,
                pending,
            ) {
                return None;
            }
            Some(((slot as u64) << 32) | u64::from(pin))
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = (
                controller_path,
                pin,
                edge_triggered,
                polarity,
                level,
                pending,
            );
            None
        }
    }
    fn set_interrupt_enabled(handle: u64, enabled: bool) -> bool {
        #[cfg(target_os = "none")]
        {
            let slot = (handle >> 32) as usize;
            let pin = (handle & 0xffff_ffff) as usize;
            let providers = PROVIDERS.lock();
            let Some(provider) = providers.get(slot) else {
                return false;
            };
            if enabled {
                provider.controller.enable_interrupt(pin)
            } else {
                provider.controller.disable_interrupt(pin)
            }
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = (handle, enabled);
            false
        }
    }
    fn release_interrupt(handle: u64) {
        #[cfg(target_os = "none")]
        {
            let slot = (handle >> 32) as usize;
            let pin = (handle & 0xffff_ffff) as usize;
            if let Some(provider) = PROVIDERS.lock().get_mut(slot) {
                let _ = provider.controller.release_interrupt(pin);
            }
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = handle;
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn alder_lake_n_pin_tables_match_upstream_group_boundaries() {
        let device = device_for_hid("INTC1057").unwrap();
        assert_eq!(device.npins, 384);
        assert_eq!(device.pad_size, 16);
        assert_eq!((device.gpi_is, device.gpi_ie), (0x100, 0x120));
        // Pin numbers are the upstream `gpiobase` space (pchgpio_find_group),
        // not the per-community pad index `base`.
        assert_eq!(find_group(device, 0).unwrap().1.bar, 0);
        assert_eq!(find_group(device, 25).unwrap().1.base, 0);
        assert!(find_group(device, 26).is_none());
        // GPP_S: bar 1, bank 0, gpiobase 96.
        assert_eq!(find_group(device, 96).unwrap().1.bank, 0);
        assert_eq!(find_group(device, 96).unwrap().1.bar, 1);
        // GPP_C: bar 2, bank 0, gpiobase 256.
        assert_eq!(find_group(device, 256).unwrap().1.bar, 2);
        // GPP_E: bar 2, bank 3, gpiobase 320.
        assert_eq!(find_group(device, 320).unwrap().1.bank, 3);
        // GPP_R: bar 3, bank 0, gpiobase 352.
        assert_eq!(find_group(device, 352).unwrap().1.bar, 3);
        assert!(find_group(device, 148).is_none());
        assert!(find_group(device, 383).is_none());
    }

    #[test]
    fn upstream_hid_table_resolves_every_supported_device() {
        assert_eq!(HIDS.len(), 14);
        assert!(device_for_hid("INTC1057").is_some());
        assert!(device_for_hid("INTC9999").is_none());
    }

    #[test]
    fn alder_lake_n_pad_read_write_and_interrupt_dispatch_use_group_offsets() {
        let mut bars: [alloc::boxed::Box<[u32]>; 4] =
            core::array::from_fn(|_| vec![0; 4096 / 4].into_boxed_slice());
        for bar in &mut bars {
            bar[3] = 0x400; // PADBAR
        }
        let addresses = [
            bars[0].as_mut_ptr() as usize,
            bars[1].as_mut_ptr() as usize,
            bars[2].as_mut_ptr() as usize,
            bars[3].as_mut_ptr() as usize,
            0,
        ];
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        let controller = unsafe { Controller::new(&ADL_N_DEVICE, addresses) }.unwrap();
        let pad0 = (addresses[0] + 0x400) as *mut u32;
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        unsafe { write_volatile(pad0, CONF_RXSTATE) };
        assert_eq!(controller.read_pin(0), Some(true));
        assert!(controller.write_pin(26, true));
        let pad26 = (addresses[0] + 0x400 + 26 * 16) as *const u32;
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        assert_ne!(unsafe { read_volatile(pad26) } & CONF_TXSTATE, 0);

        let pending = alloc::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
        let mut controller = controller;
        assert!(controller.establish_interrupt(0, true, true, false, 3, pending.clone()));
        assert!(controller.enable_interrupt(0));
        // SAFETY: this pointer is derived from a checked MMIO window or test BAR backing.
        unsafe {
            write_volatile((addresses[0] + 0x100) as *mut u32, 1);
        }
        assert!(controller.dispatch_interrupt());
        assert!(pending.load(core::sync::atomic::Ordering::Acquire));
    }
}
