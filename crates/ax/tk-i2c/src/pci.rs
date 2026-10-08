//! PCI bus attachment translation for FreeBSD `sys/dev/ichiic/ig4_pci.c`.
//! FreeBSD source snapshot 2026-10-08; BSD-3-Clause.
//! Copyright (c) 2014 The DragonFly Project. Full notice in `LICENSES/BSD-3-Clause.txt`.
use crate::ig4::{Backend, Ig4, IicError, Version, Version::*};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciMatch {
    pub device_id: u16,
    pub description: &'static str,
    pub version: Version,
}

/// PCI match table translated row-for-row from `ig4iic_pci_devices`.
pub const PCI_DEVICES: &[PciMatch] = &[
    PciMatch {
        device_id: 0x0f41,
        description: "Intel BayTrail Serial I/O I2C Port 1",
        version: Atom,
    },
    PciMatch {
        device_id: 0x0f42,
        description: "Intel BayTrail Serial I/O I2C Port 2",
        version: Atom,
    },
    PciMatch {
        device_id: 0x0f43,
        description: "Intel BayTrail Serial I/O I2C Port 3",
        version: Atom,
    },
    PciMatch {
        device_id: 0x0f44,
        description: "Intel BayTrail Serial I/O I2C Port 4",
        version: Atom,
    },
    PciMatch {
        device_id: 0x0f45,
        description: "Intel BayTrail Serial I/O I2C Port 5",
        version: Atom,
    },
    PciMatch {
        device_id: 0x0f46,
        description: "Intel BayTrail Serial I/O I2C Port 6",
        version: Atom,
    },
    PciMatch {
        device_id: 0x0f47,
        description: "Intel BayTrail Serial I/O I2C Port 7",
        version: Atom,
    },
    PciMatch {
        device_id: 0x9c61,
        description: "Intel Lynx Point-LP I2C Controller-1",
        version: Haswell,
    },
    PciMatch {
        device_id: 0x9c62,
        description: "Intel Lynx Point-LP I2C Controller-2",
        version: Haswell,
    },
    PciMatch {
        device_id: 0x22c1,
        description: "Intel Braswell Serial I/O I2C Port 1",
        version: Atom,
    },
    PciMatch {
        device_id: 0x22c2,
        description: "Intel Braswell Serial I/O I2C Port 2",
        version: Atom,
    },
    PciMatch {
        device_id: 0x22c3,
        description: "Intel Braswell Serial I/O I2C Port 3",
        version: Atom,
    },
    PciMatch {
        device_id: 0x22c5,
        description: "Intel Braswell Serial I/O I2C Port 5",
        version: Atom,
    },
    PciMatch {
        device_id: 0x22c6,
        description: "Intel Braswell Serial I/O I2C Port 6",
        version: Atom,
    },
    PciMatch {
        device_id: 0x22c7,
        description: "Intel Braswell Serial I/O I2C Port 7",
        version: Atom,
    },
    PciMatch {
        device_id: 0x9d60,
        description: "Intel Sunrise Point-LP I2C Controller-0",
        version: Skylake,
    },
    PciMatch {
        device_id: 0x9d61,
        description: "Intel Sunrise Point-LP I2C Controller-1",
        version: Skylake,
    },
    PciMatch {
        device_id: 0x9d62,
        description: "Intel Sunrise Point-LP I2C Controller-2",
        version: Skylake,
    },
    PciMatch {
        device_id: 0x9d63,
        description: "Intel Sunrise Point-LP I2C Controller-3",
        version: Skylake,
    },
    PciMatch {
        device_id: 0x9d64,
        description: "Intel Sunrise Point-LP I2C Controller-4",
        version: Skylake,
    },
    PciMatch {
        device_id: 0x9d65,
        description: "Intel Sunrise Point-LP I2C Controller-5",
        version: Skylake,
    },
    PciMatch {
        device_id: 0xa160,
        description: "Intel Sunrise Point-H I2C Controller-0",
        version: Skylake,
    },
    PciMatch {
        device_id: 0xa161,
        description: "Intel Sunrise Point-H I2C Controller-1",
        version: Skylake,
    },
    PciMatch {
        device_id: 0x5aac,
        description: "Intel Apollo Lake I2C Controller-0",
        version: ApolloLake,
    },
    PciMatch {
        device_id: 0x5aae,
        description: "Intel Apollo Lake I2C Controller-1",
        version: ApolloLake,
    },
    PciMatch {
        device_id: 0x5ab0,
        description: "Intel Apollo Lake I2C Controller-2",
        version: ApolloLake,
    },
    PciMatch {
        device_id: 0x5ab2,
        description: "Intel Apollo Lake I2C Controller-3",
        version: ApolloLake,
    },
    PciMatch {
        device_id: 0x5ab4,
        description: "Intel Apollo Lake I2C Controller-4",
        version: ApolloLake,
    },
    PciMatch {
        device_id: 0x5ab6,
        description: "Intel Apollo Lake I2C Controller-5",
        version: ApolloLake,
    },
    PciMatch {
        device_id: 0x5ab8,
        description: "Intel Apollo Lake I2C Controller-6",
        version: ApolloLake,
    },
    PciMatch {
        device_id: 0x5aba,
        description: "Intel Apollo Lake I2C Controller-7",
        version: ApolloLake,
    },
    PciMatch {
        device_id: 0x9dc5,
        description: "Intel Cannon Lake-LP I2C Controller-0",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x9dc6,
        description: "Intel Cannon Lake-LP I2C Controller-1",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x9de8,
        description: "Intel Cannon Lake-LP I2C Controller-2",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x9de9,
        description: "Intel Cannon Lake-LP I2C Controller-3",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x9dea,
        description: "Intel Cannon Lake-LP I2C Controller-4",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x9deb,
        description: "Intel Cannon Lake-LP I2C Controller-5",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0xa368,
        description: "Intel Cannon Lake-H I2C Controller-0",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0xa369,
        description: "Intel Cannon Lake-H I2C Controller-1",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0xa36a,
        description: "Intel Cannon Lake-H I2C Controller-2",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0xa36b,
        description: "Intel Cannon Lake-H I2C Controller-3",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x02e8,
        description: "Intel Comet Lake-LP I2C Controller-0",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x02e9,
        description: "Intel Comet Lake-LP I2C Controller-1",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x02ea,
        description: "Intel Comet Lake-LP I2C Controller-2",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x02eb,
        description: "Intel Comet Lake-LP I2C Controller-3",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x02c5,
        description: "Intel Comet Lake-LP I2C Controller-4",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x02c6,
        description: "Intel Comet Lake-LP I2C Controller-5",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x06e8,
        description: "Intel Comet Lake-H I2C Controller-0",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x06e9,
        description: "Intel Comet Lake-H I2C Controller-1",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x06ea,
        description: "Intel Comet Lake-H I2C Controller-2",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x06eb,
        description: "Intel Comet Lake-H I2C Controller-3",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0xa3e0,
        description: "Intel Comet Lake-V I2C Controller-0",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0xa3e1,
        description: "Intel Comet Lake-V I2C Controller-1",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0xa3e2,
        description: "Intel Comet Lake-V I2C Controller-2",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0xa3e3,
        description: "Intel Comet Lake-V I2C Controller-3",
        version: CannonLake,
    },
    PciMatch {
        device_id: 0x34e8,
        description: "Intel Ice Lake-LP I2C Controller-0",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x34e9,
        description: "Intel Ice Lake-LP I2C Controller-1",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x34ea,
        description: "Intel Ice Lake-LP I2C Controller-2",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x34eb,
        description: "Intel Ice Lake-LP I2C Controller-3",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x34c5,
        description: "Intel Ice Lake-LP I2C Controller-4",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x34c6,
        description: "Intel Ice Lake-LP I2C Controller-5",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x43d8,
        description: "Intel Tiger Lake-H I2C Controller-0",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x43e8,
        description: "Intel Tiger Lake-H I2C Controller-1",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x43e9,
        description: "Intel Tiger Lake-H I2C Controller-2",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x43ea,
        description: "Intel Tiger Lake-H I2C Controller-3",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x43eb,
        description: "Intel Tiger Lake-H I2C Controller-4",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x43ad,
        description: "Intel Tiger Lake-H I2C Controller-5",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x43ae,
        description: "Intel Tiger Lake-H I2C Controller-6",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0xa0c5,
        description: "Intel Tiger Lake-LP I2C Controller-0",
        version: Skylake,
    },
    PciMatch {
        device_id: 0xa0c6,
        description: "Intel Tiger Lake-LP I2C Controller-1",
        version: Skylake,
    },
    PciMatch {
        device_id: 0xa0d8,
        description: "Intel Tiger Lake-LP I2C Controller-2",
        version: Skylake,
    },
    PciMatch {
        device_id: 0xa0d9,
        description: "Intel Tiger Lake-LP I2C Controller-3",
        version: Skylake,
    },
    PciMatch {
        device_id: 0xa0e8,
        description: "Intel Tiger Lake-LP I2C Controller-4",
        version: Skylake,
    },
    PciMatch {
        device_id: 0xa0e9,
        description: "Intel Tiger Lake-LP I2C Controller-5",
        version: Skylake,
    },
    PciMatch {
        device_id: 0xa0ea,
        description: "Intel Tiger Lake-LP I2C Controller-6",
        version: Skylake,
    },
    PciMatch {
        device_id: 0xa0eb,
        description: "Intel Tiger Lake-LP I2C Controller-7",
        version: Skylake,
    },
    PciMatch {
        device_id: 0x31ac,
        description: "Intel Gemini Lake I2C Controller-0",
        version: GeminiLake,
    },
    PciMatch {
        device_id: 0x31ae,
        description: "Intel Gemini Lake I2C Controller-1",
        version: GeminiLake,
    },
    PciMatch {
        device_id: 0x31b0,
        description: "Intel Gemini Lake I2C Controller-2",
        version: GeminiLake,
    },
    PciMatch {
        device_id: 0x31b2,
        description: "Intel Gemini Lake I2C Controller-3",
        version: GeminiLake,
    },
    PciMatch {
        device_id: 0x31b4,
        description: "Intel Gemini Lake I2C Controller-4",
        version: GeminiLake,
    },
    PciMatch {
        device_id: 0x31b6,
        description: "Intel Gemini Lake I2C Controller-5",
        version: GeminiLake,
    },
    PciMatch {
        device_id: 0x31b8,
        description: "Intel Gemini Lake I2C Controller-6",
        version: GeminiLake,
    },
    PciMatch {
        device_id: 0x31ba,
        description: "Intel Gemini Lake I2C Controller-7",
        version: GeminiLake,
    },
    PciMatch {
        device_id: 0x4de8,
        description: "Intel Jasper Lake I2C Controller-0",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x4de9,
        description: "Intel Jasper Lake I2C Controller-1",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x4dea,
        description: "Intel Jasper Lake I2C Controller-2",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x4deb,
        description: "Intel Jasper Lake I2C Controller-3",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x4dc5,
        description: "Intel Jasper Lake I2C Controller-4",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x4dc6,
        description: "Intel Jasper Lake I2C Controller-5",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x51e8,
        description: "Intel Alder Lake-P I2C Controller-0",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x51e9,
        description: "Intel Alder Lake-P I2C Controller-1",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x51ea,
        description: "Intel Alder Lake-P I2C Controller-2",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x51eb,
        description: "Intel Alder Lake-P I2C Controller-3",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x51c5,
        description: "Intel Alder Lake-P I2C Controller-4",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x51c6,
        description: "Intel Alder Lake-P I2C Controller-5",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x51d8,
        description: "Intel Alder Lake-P I2C Controller-6",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x51d9,
        description: "Intel Alder Lake-P I2C Controller-7",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7acc,
        description: "Intel Alder Lake-S I2C Controller-0",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7acd,
        description: "Intel Alder Lake-S I2C Controller-1",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7ace,
        description: "Intel Alder Lake-S I2C Controller-2",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7acf,
        description: "Intel Alder Lake-S I2C Controller-3",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7afc,
        description: "Intel Alder Lake-S I2C Controller-4",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7afd,
        description: "Intel Alder Lake-S I2C Controller-5",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x54e8,
        description: "Intel Alder Lake-M I2C Controller-0",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x54e9,
        description: "Intel Alder Lake-M I2C Controller-1",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x54ea,
        description: "Intel Alder Lake-M I2C Controller-2",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x54eb,
        description: "Intel Alder Lake-M I2C Controller-3",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x54c5,
        description: "Intel Alder Lake-M I2C Controller-4",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x54c6,
        description: "Intel Alder Lake-M I2C Controller-5",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7a4c,
        description: "Intel Raptor Lake-S I2C Controller-0",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7a4d,
        description: "Intel Raptor Lake-S I2C Controller-1",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7a4e,
        description: "Intel Raptor Lake-S I2C Controller-2",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7a4f,
        description: "Intel Raptor Lake-S I2C Controller-3",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7a7c,
        description: "Intel Raptor Lake-S I2C Controller-4",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7a7d,
        description: "Intel Raptor Lake-S I2C Controller-5",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7e78,
        description: "Intel Meteor Lake-M I2C Controller-0",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7e79,
        description: "Intel Meteor Lake-M I2C Controller-1",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7e50,
        description: "Intel Meteor Lake-M I2C Controller-2",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7e51,
        description: "Intel Meteor Lake-M I2C Controller-3",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7e7a,
        description: "Intel Meteor Lake-M I2C Controller-4",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7e7b,
        description: "Intel Meteor Lake-M I2C Controller-5",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7778,
        description: "Intel Arrow Lake-H/U I2C Controller-0",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7779,
        description: "Intel Arrow Lake-H/U I2C Controller-1",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x777a,
        description: "Intel Arrow Lake-H/U I2C Controller-2",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x777b,
        description: "Intel Arrow Lake-H/U I2C Controller-3",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7750,
        description: "Intel Arrow Lake-H/U I2C Controller-4",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0x7751,
        description: "Intel Arrow Lake-H/U I2C Controller-5",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0xa878,
        description: "Intel Lunar Lake-M I2C Controller-0",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0xa879,
        description: "Intel Lunar Lake-M I2C Controller-1",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0xa87a,
        description: "Intel Lunar Lake-M I2C Controller-2",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0xa87b,
        description: "Intel Lunar Lake-M I2C Controller-3",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0xa850,
        description: "Intel Lunar Lake-M I2C Controller-4",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0xa851,
        description: "Intel Lunar Lake-M I2C Controller-5",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0xe478,
        description: "Intel Panther Lake-H I2C Controller-0",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0xe479,
        description: "Intel Panther Lake-H I2C Controller-1",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0xe450,
        description: "Intel Panther Lake-H I2C Controller-2",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0xe451,
        description: "Intel Panther Lake-H I2C Controller-3",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0xe47a,
        description: "Intel Panther Lake-H I2C Controller-4",
        version: TigerLake,
    },
    PciMatch {
        device_id: 0xe47b,
        description: "Intel Panther Lake-H I2C Controller-5",
        version: TigerLake,
    },
];

// upstream: ig4_pci.c ig4iic_pci_probe()
pub fn ig4iic_pci_probe(vendor_id: u16, device_id: u16) -> Option<&'static PciMatch> {
    PCI_DEVICES
        .iter()
        .find(|entry| vendor_id == 0x8086 && entry.device_id == device_id)
}

pub trait PciResources: Backend {
    fn allocate_bar0_memory(&mut self) -> bool;
    fn allocate_msi(&mut self) -> bool;
    fn allocate_irq(&mut self, msi_rid: u8) -> bool;
    fn release_irq(&mut self, rid: u8);
    fn release_msi(&mut self);
    fn release_bar0(&mut self);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PciAttachment {
    pub platform_attached: bool,
    pub intr_rid: u8,
    pub msi_allocated: bool,
    pub regs_allocated: bool,
    pub irq_allocated: bool,
}

// upstream: ig4_pci.c ig4iic_pci_attach()
pub fn ig4iic_pci_attach<P: PciResources>(
    sc: &mut Ig4<P>,
    state: &mut PciAttachment,
) -> Result<(), IicError> {
    state.regs_allocated = sc.io.allocate_bar0_memory();
    if !state.regs_allocated {
        let _ = ig4iic_pci_detach(sc, state);
        return Err(IicError::Invalid);
    }
    state.intr_rid = 0;
    state.msi_allocated = sc.io.allocate_msi();
    if state.msi_allocated {
        state.intr_rid = 1;
    }
    state.irq_allocated = sc.io.allocate_irq(state.intr_rid);
    if !state.irq_allocated {
        let _ = ig4iic_pci_detach(sc, state);
        return Err(IicError::Invalid);
    }
    state.platform_attached = true;
    if sc.ig4iic_attach().is_err() {
        let _ = ig4iic_pci_detach(sc, state);
        return Err(IicError::Invalid);
    }
    Ok(())
}

// upstream: ig4_pci.c ig4iic_pci_detach()
pub fn ig4iic_pci_detach<P: PciResources>(
    sc: &mut Ig4<P>,
    state: &mut PciAttachment,
) -> Result<(), IicError> {
    if state.platform_attached {
        sc.ig4iic_detach()?;
        state.platform_attached = false;
    }
    if state.irq_allocated {
        sc.io.release_irq(state.intr_rid);
        state.irq_allocated = false;
    }
    if state.msi_allocated {
        sc.io.release_msi();
        state.msi_allocated = false;
    }
    if state.regs_allocated {
        sc.io.release_bar0();
        state.regs_allocated = false;
    }
    Ok(())
}

// upstream: ig4_pci.c ig4iic_pci_suspend()
pub fn ig4iic_pci_suspend<P: Backend>(sc: &mut Ig4<P>) -> Result<(), IicError> {
    sc.ig4iic_suspend()
}

// upstream: ig4_pci.c ig4iic_pci_resume()
pub fn ig4iic_pci_resume<P: Backend>(sc: &mut Ig4<P>) -> Result<(), IicError> {
    sc.ig4iic_resume()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn n305_lpss_ids_match_tigerlake() {
        assert_eq!(ig4iic_pci_probe(0x8086, 0x54e8).unwrap().version, TigerLake);
        assert_eq!(ig4iic_pci_probe(0x8086, 0x54ea).unwrap().version, TigerLake);
        assert!(ig4iic_pci_probe(0x8086, 0xffff).is_none());
    }
}
