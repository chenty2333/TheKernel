pub const RELOAD: u16 = 0;
pub const STATUS1: u16 = 4;
pub const STATUS2: u16 = 6;
pub const CONTROL1: u16 = 8;
pub const TIMER: u16 = 0x12;
pub const HALT: u16 = 1 << 11;
pub const NMI_NOW: u16 = 1 << 8;
pub const BOOT_STATUS: u16 = 1 << 2;
pub trait Bus: Send {
    fn read16(&mut self, offset: u16) -> u16;
    fn write16(&mut self, offset: u16, value: u16);
    fn read_no_reboot(&mut self) -> u32;
    fn write_no_reboot(&mut self, value: u32);
    fn read_smi_enable(&mut self) -> u32;
    fn write_smi_enable(&mut self, value: u32);
}
