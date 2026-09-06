#[cfg(bus = "mmio")]
mod mmio;
#[cfg(bus = "pci")]
pub(crate) mod pci;
