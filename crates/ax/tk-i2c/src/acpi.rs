//! ACPI bus glue translated from FreeBSD `sys/dev/ichiic/ig4_acpi.c`.
//! FreeBSD source snapshot 2026-10-08; BSD-2-Clause.
//! Copyright (c) 2016 Oleksandr Tymoshenko <gonzo@FreeBSD.org>.
//! Full notice is retained in `LICENSES/BSD-2-Clause.txt`.

use crate::ig4::{Backend, Ig4, IicError, Version};

pub const ACPI_IDS: &[&str] = &[
    "INT33C2", "INT33C3", "INT3432", "INT3433", "INT3446", "80860F41", "808622C1", "AMDI0510",
    "AMDI0010", "AMD0010", "APMC0D0F",
];

// upstream: ig4_acpi.c ig4iic_acpi_probe()
pub fn ig4iic_acpi_probe(hid: &str, disabled: bool) -> Result<&'static str, IicError> {
    if disabled || !ACPI_IDS.contains(&hid) {
        return Err(IicError::Invalid);
    }
    Ok("Designware I2C Controller")
}

// upstream: ig4_acpi.c ig4iic_acpi_attach()
pub fn ig4iic_acpi_attach<A: AcpiResources>(
    sc: &mut Ig4<A>,
    state: &mut AcpiAttachment,
    hid: &str,
) -> Result<(), IicError> {
    if !ACPI_IDS.contains(&hid) {
        return Err(IicError::Invalid);
    }
    sc.version = match hid {
        "APMC0D0F" => Version::Emag,
        "INT33C2" | "INT33C3" | "INT3432" | "INT3433" => Version::Haswell,
        _ => Version::Atom,
    };
    // upstream ignores the result of Device_SetPowerState(D0).
    let _ = sc.io.set_power_d0();
    state.regs_allocated = sc.io.allocate_memory().is_ok();
    if !state.regs_allocated {
        let _ = ig4iic_acpi_detach(sc, state);
        return Err(IicError::NoDevice);
    }
    state.intr_allocated = sc.io.allocate_irq().is_ok();
    if !state.intr_allocated {
        let _ = ig4iic_acpi_detach(sc, state);
        return Err(IicError::NoDevice);
    }
    state.platform_attached = true;
    if sc.ig4iic_attach().is_err() {
        let _ = ig4iic_acpi_detach(sc, state);
        return Err(IicError::NoDevice);
    }
    Ok(())
}

// upstream: ig4_acpi.c ig4iic_acpi_detach()
pub fn ig4iic_acpi_detach<A: AcpiResources>(
    sc: &mut Ig4<A>,
    state: &mut AcpiAttachment,
) -> Result<(), IicError> {
    if state.platform_attached {
        sc.ig4iic_detach()?;
        state.platform_attached = false;
    }
    if state.intr_allocated {
        sc.io.release_irq();
        state.intr_allocated = false;
    }
    if state.regs_allocated {
        sc.io.release_memory();
        state.regs_allocated = false;
    }
    Ok(())
}

// upstream: ig4_acpi.c ig4iic_acpi_suspend()
pub fn ig4iic_acpi_suspend<A: Backend>(sc: &mut Ig4<A>) -> Result<(), IicError> {
    sc.ig4iic_suspend()
}

// upstream: ig4_acpi.c ig4iic_acpi_resume()
pub fn ig4iic_acpi_resume<A: Backend>(sc: &mut Ig4<A>) -> Result<(), IicError> {
    sc.ig4iic_resume()
}

pub trait AcpiResources: Backend {
    fn set_power_d0(&mut self) -> Result<(), IicError>;
    fn allocate_memory(&mut self) -> Result<(), IicError>;
    fn allocate_irq(&mut self) -> Result<(), IicError>;
    fn release_irq(&mut self);
    fn release_memory(&mut self);
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AcpiAttachment {
    pub platform_attached: bool,
    pub regs_allocated: bool,
    pub intr_allocated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn probe_filters_disable_and_recognizes_upstream_ids() {
        assert_eq!(
            ig4iic_acpi_probe("INT33C2", false),
            Ok("Designware I2C Controller")
        );
        assert!(ig4iic_acpi_probe("INT33C2", true).is_err());
        assert!(ig4iic_acpi_probe("PNP0C50", false).is_err());
    }
    #[test]
    fn upstream_hids_select_platform_generation() {
        assert_eq!(ACPI_IDS.len(), 11);
        assert_eq!(Version::Emag as u8, 0);
        assert_eq!(Version::Haswell as u8, 1);
        assert_eq!(Version::Atom as u8, 2);
    }
}
