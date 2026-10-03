//! Register facts: Linux r8169_main.c:257-315, 411-455, 473-501.
//! IRQ layout is selected by ids::Chip; 8168 uses word, 8125 dword registers.
pub const WINDOW: usize = 0x10000;
pub const MAC: usize = 0x00;
pub const TX_LOW: usize = 0x20;
pub const TX_HIGH: usize = 0x24;
pub const INT_CFG: usize = 0x34;
pub const COMMAND: usize = 0x37;
pub const IRQ_MASK: usize = 0x38;
pub const IRQ_STATUS: usize = 0x3c;
pub const TX_CONFIG: usize = 0x40;
pub const RX_CONFIG: usize = 0x44;
pub const CFG_LOCK: usize = 0x50;
pub const PHY_STATUS: usize = 0x6c;
pub const INT_CFG1: usize = 0x7a;
pub const TX_POLL: usize = 0x90;
pub const OCP_DATA: usize = 0xb0;
pub const RX_MAX: usize = 0xda;
pub const CPLUS: usize = 0xe0;
pub const RX_LOW: usize = 0xe4;
pub const RX_HIGH: usize = 0xe8;
pub const MISC: usize = 0xf0;
pub const RSS: usize = 0x4500;
pub const QUEUES: usize = 0x4800;
pub const RESET: u32 = 0x10;
pub const RX_TX_ENABLE: u32 = 0x0c;
pub const LINK: u32 = 2;

/// Access width is explicit: a dword at ChipCmd would overwrite the 8125 mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    Byte,
    Word,
    Dword,
}
pub trait Bus: Send + Sync {
    fn read(&mut self, offset: usize, width: Width) -> u32;
    fn write(&mut self, offset: usize, width: Width, value: u32);
    fn delay_us(&mut self, micros: u32);
    fn irq_num(&self) -> Option<usize> {
        None
    }
    fn interrupts_available(&self) -> bool {
        false
    }
}
