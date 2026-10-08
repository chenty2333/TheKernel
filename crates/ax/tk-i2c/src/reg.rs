//! Registers and bit fields translated from FreeBSD `sys/dev/ichiic/ig4_reg.h`.
//! FreeBSD source snapshot 2026-10-08; BSD-3-Clause.
//! Copyright (c) 2014 The DragonFly Project. Full notice in `LICENSES/BSD-3-Clause.txt`.

pub const IG4_REG_CTL: u32 = 0x0000; // RW	Control Register
pub const IG4_REG_TAR_ADD: u32 = 0x0004; // RW	Target Address
pub const IG4_REG_HS_MADDR: u32 = 0x000C; // RW	High Speed Master Mode Code Address
pub const IG4_REG_DATA_CMD: u32 = 0x0010; // RW	Data Buffer and Command
pub const IG4_REG_SS_SCL_HCNT: u32 = 0x0014; // RW	Std Speed clock High Count
pub const IG4_REG_SS_SCL_LCNT: u32 = 0x0018; // RW	Std Speed clock Low Count
pub const IG4_REG_FS_SCL_HCNT: u32 = 0x001C; // RW	Fast Speed clock High Count
pub const IG4_REG_FS_SCL_LCNT: u32 = 0x0020; // RW	Fast Speed clock Low Count
pub const IG4_REG_INTR_STAT: u32 = 0x002C; // RO	Interrupt Status
pub const IG4_REG_INTR_MASK: u32 = 0x0030; // RW	Interrupt Mask
pub const IG4_REG_RAW_INTR_STAT: u32 = 0x0034; // RO	Raw Interrupt Status
pub const IG4_REG_RX_TL: u32 = 0x0038; // RW	Receive FIFO Threshold
pub const IG4_REG_TX_TL: u32 = 0x003C; // RW	Transmit FIFO Threshold
pub const IG4_REG_CLR_INTR: u32 = 0x0040; // RO	Clear Interrupt
pub const IG4_REG_CLR_RX_UNDER: u32 = 0x0044; // RO	Clear RX_Under Interrupt
pub const IG4_REG_CLR_RX_OVER: u32 = 0x0048; // RO	Clear RX_Over Interrupt
pub const IG4_REG_CLR_TX_OVER: u32 = 0x004C; // RO	Clear TX_Over Interrupt
pub const IG4_REG_CLR_RD_REQ: u32 = 0x0050; // RO	Clear RD_Req Interrupt
pub const IG4_REG_CLR_TX_ABORT: u32 = 0x0054; // RO	Clear TX_Abort Interrupt
pub const IG4_REG_CLR_RX_DONE: u32 = 0x0058; // RO	Clear RX_Done Interrupt
pub const IG4_REG_CLR_ACTIVITY: u32 = 0x005C; // RO	Clear Activity Interrupt
pub const IG4_REG_CLR_STOP_DET: u32 = 0x0060; // RO	Clear STOP Detection Int
pub const IG4_REG_CLR_START_DET: u32 = 0x0064; // RO	Clear START Detection Int
pub const IG4_REG_CLR_GEN_CALL: u32 = 0x0068; // RO	Clear General Call Interrupt
pub const IG4_REG_I2C_EN: u32 = 0x006C; // RW	I2C Enable
pub const IG4_REG_I2C_STA: u32 = 0x0070; // RO	I2C Status
pub const IG4_REG_TXFLR: u32 = 0x0074; // RO	Transmit FIFO Level
pub const IG4_REG_RXFLR: u32 = 0x0078; // RO	Receive FIFO Level
pub const IG4_REG_SDA_HOLD: u32 = 0x007C; // RW	SDA Hold Time Length
pub const IG4_REG_TX_ABRT_SOURCE: u32 = 0x0080; // RO	Transmit Abort Source
pub const IG4_REG_SLV_DATA_NACK: u32 = 0x0084; // RW	General Slave Data NACK
pub const IG4_REG_DMA_CTRL: u32 = 0x0088; // RW	DMA Control
pub const IG4_REG_DMA_TDLR: u32 = 0x008C; // RW	DMA Transmit Data Level
pub const IG4_REG_DMA_RDLR: u32 = 0x0090; // RW	DMA Receive Data Level
pub const IG4_REG_SDA_SETUP: u32 = 0x0094; // RW	SDA Setup
pub const IG4_REG_ACK_GENERAL_CALL: u32 = 0x0098; // RW	I2C ACK General Call
pub const IG4_REG_ENABLE_STATUS: u32 = 0x009C; // RO	Enable Status
pub const IG4_REG_COMP_PARAM1: u32 = 0x00F4; // RO	Component Parameter
pub const IG4_REG_COMP_VER: u32 = 0x00F8; // RO	Component Version
pub const IG4_REG_COMP_TYPE: u32 = 0x00FC; // RO	Probe width/endian? (linux)
pub const IG4_REG_RESETS_SKL: u32 = 0x0204; // RW	Reset Register
pub const IG4_REG_ACTIVE_LTR_VALUE: u32 = 0x0210; // RW	Active LTR Value
pub const IG4_REG_IDLE_LTR_VALUE: u32 = 0x0214; // RW	Idle LTR Value
pub const IG4_REG_TX_ACK_COUNT: u32 = 0x0218; // RO	TX ACK Count
pub const IG4_REG_RX_BYTE_COUNT: u32 = 0x021C; // RO	RX ACK Count
pub const IG4_REG_DEVIDLE_CTRL: u32 = 0x024C; // RW	Device Control
pub const IG4_REG_CLK_PARMS: u32 = 0x0800; // RW	Clock Parameters
pub const IG4_CLK_PARMS_EN: u32 = 0x00000001; // functional clock ungated
pub const IG4_REG_RESETS_HSW: u32 = 0x0804; // RW	Reset Register
pub const IG4_REG_GENERAL: u32 = 0x0808; // RW	General Register
pub const IG4_REG_SW_LTR_VALUE: u32 = 0x0810; // RW	SW LTR Value
pub const IG4_REG_AUTO_LTR_VALUE: u32 = 0x0814; // RW	Auto LTR Value
pub const IG4_CTL_SLAVE_DISABLE: u32 = 0x0040; // snarfed from linux
pub const IG4_CTL_RESTARTEN: u32 = 0x0020; // Allow Restart when master
pub const IG4_CTL_10BIT: u32 = 0x0010; // ctlr accepts 10-bit addresses
pub const IG4_CTL_SPEED_MASK: u32 = 0x0006; // speed at which the I2C operates
pub const IG4_CTL_MASTER: u32 = 0x0001; // snarfed from linux
pub const IG4_CTL_SPEED_HIGH: u32 = 0x0006; // snarfed from linux
pub const IG4_CTL_SPEED_FAST: u32 = 0x0004; // snarfed from linux
pub const IG4_CTL_SPEED_STD: u32 = 0x0002; // snarfed from linux
pub const IG4_TAR_10BIT: u32 = 0x1000; // start xfer in 10-bit mode
pub const IG4_TAR_SPECIAL: u32 = 0x0800; // Perform special command
pub const IG4_TAR_GC_OR_START: u32 = 0x0400; // General Call or Start
pub const IG4_TAR_ADDR_MASK: u32 = 0x03FF; // Target address
pub const IG4_DATA_RESTART: u32 = 0x0400; // Force RESTART
pub const IG4_DATA_STOP: u32 = 0x0200; // Force STOP[+START]
pub const IG4_DATA_COMMAND_RD: u32 = 0x0100; // bus direction 0=write 1=read
pub const IG4_DATA_MASK: u32 = 0x00FF;
pub const IG4_SCL_CLOCK_MASK: u32 = 0xFFFF; // count bits in register
pub const IG4_INTR_GEN_CALL: u32 = 0x0800;
pub const IG4_INTR_START_DET: u32 = 0x0400;
pub const IG4_INTR_STOP_DET: u32 = 0x0200;
pub const IG4_INTR_ACTIVITY: u32 = 0x0100;
pub const IG4_INTR_TX_ABRT: u32 = 0x0040;
pub const IG4_INTR_TX_EMPTY: u32 = 0x0010;
pub const IG4_INTR_TX_OVER: u32 = 0x0008;
pub const IG4_INTR_RX_FULL: u32 = 0x0004;
pub const IG4_INTR_RX_OVER: u32 = 0x0002;
pub const IG4_INTR_RX_UNDER: u32 = 0x0001;
pub const IG4_FIFO_MASK: u32 = 0x00FF;
pub const IG4_FIFO_LIMIT: u32 = 16;
pub const IG4_CLR_BIT: u32 = 0x0001; // Reflects source
pub const IG4_I2C_ABORT: u32 = 0x0002;
pub const IG4_I2C_ENABLE: u32 = 0x0001;
pub const IG4_STATUS_ACTIVITY: u32 = 0x0020; // Controller is active
pub const IG4_STATUS_RX_FULL: u32 = 0x0010; // RX FIFO completely full
pub const IG4_STATUS_RX_NOTEMPTY: u32 = 0x0008; // RX FIFO not empty
pub const IG4_STATUS_TX_EMPTY: u32 = 0x0004; // TX FIFO completely empty
pub const IG4_STATUS_TX_NOTFULL: u32 = 0x0002; // TX FIFO not full
pub const IG4_STATUS_I2C_ACTIVE: u32 = 0x0001; // I2C bus is active
pub const IG4_FIFOLVL_MASK: u32 = 0x01FF;
pub const IG4_SDA_TX_HOLD_MASK: u32 = 0x0000FFFF;
pub const IG4_ABRTSRC_TRANSFER: u32 = 0x00010000; // Abort initiated by user
pub const IG4_ABRTSRC_ARBLOST: u32 = 0x00001000; // Arbitration lost
pub const IG4_ABRTSRC_NORESTART_10: u32 = 0x00000400; // RESTART disabled
pub const IG4_ABRTSRC_NORESTART_START: u32 = 0x00000200; // RESTART disabled
pub const IG4_ABRTSRC_ACKED_START: u32 = 0x00000080; // Improper acked START
pub const IG4_ABRTSRC_GENCALL_READ: u32 = 0x00000020; // Improper GENCALL
pub const IG4_ABRTSRC_GENCALL_NOACK: u32 = 0x00000010; // Nobody acked GENCALL
pub const IG4_ABRTSRC_TXNOACK_DATA: u32 = 0x00000008; // data phase no ACK
pub const IG4_ABRTSRC_TXNOACK_ADDR10_2: u32 = 0x00000004; // addr10/1 phase no ACK
pub const IG4_ABRTSRC_TXNOACK_ADDR10_1: u32 = 0x00000002; // addr10/2 phase no ACK
pub const IG4_ABRTSRC_TXNOACK_ADDR7: u32 = 0x00000001; // addr7 phase no ACK
pub const IG4_NACK_GENERATE: u32 = 0x0001;
pub const IG4_TX_DMA_ENABLE: u32 = 0x0002;
pub const IG4_RX_DMA_ENABLE: u32 = 0x0001;
pub const IG4_SDA_SETUP_MASK: u32 = 0x00FF;
pub const IG4_ACKGC_ACK: u32 = 0x0001;
pub const IG4_ENASTAT_DATA_LOST: u32 = 0x0004;
pub const IG4_ENASTAT_ENABLED: u32 = 0x0001;
pub const IG4_PARAM1_CONFIG_VALID: u32 = 0x00000080;
pub const IG4_PARAM1_CONFIG_HASDMA: u32 = 0x00000040;
pub const IG4_PARAM1_CONFIG_INTR_IO: u32 = 0x00000020;
pub const IG4_PARAM1_CONFIG_HCCNT_RO: u32 = 0x00000010;
pub const IG4_PARAM1_CONFIG_MAXSPEED_MASK: u32 = 0x0000000C;
pub const IG4_PARAM1_CONFIG_DATAW_MASK: u32 = 0x00000003;
pub const IG4_CONFIG_MAXSPEED_RESERVED00: u32 = 0x00000000;
pub const IG4_CONFIG_MAXSPEED_STANDARD: u32 = 0x00000004;
pub const IG4_CONFIG_MAXSPEED_FAST: u32 = 0x00000008;
pub const IG4_CONFIG_MAXSPEED_HIGH: u32 = 0x0000000C;
pub const IG4_CONFIG_DATAW_8: u32 = 0x00000000;
pub const IG4_CONFIG_DATAW_16: u32 = 0x00000001;
pub const IG4_CONFIG_DATAW_32: u32 = 0x00000002;
pub const IG4_CONFIG_DATAW_RESERVED11: u32 = 0x00000003;
pub const IG4_COMP_MIN_VER: u32 = 0x3131352A;
pub const IG4_COMP_TYPE: u32 = 0x44570140;
pub const IG4_RESETS_ASSERT_HSW: u32 = 0x0003;
pub const IG4_RESETS_DEASSERT_HSW: u32 = 0x0000;
pub const IG4_RESETS_DEASSERT_SKL: u32 = 0x0003;
pub const IG4_RESETS_ASSERT_SKL: u32 = 0x0000;
pub const IG4_RESTORE_REQUIRED: u32 = 0x0008;
pub const IG4_DEVICE_IDLE: u32 = 0x0004;
pub const IG4_GENERAL_IOVOLT3_3: u32 = 0x0008;
pub const IG4_GENERAL_SWMODE: u32 = 0x0004;
pub const IG4_SWLTR_NSNOOP_REQ: u32 = 0x80000000; // (ro)
pub const IG4_SWLTR_NSNOOP_SCALE_MASK: u32 = 0x1C000000; // (ro)
pub const IG4_SWLTR_NSNOOP_SCALE_1US: u32 = 0x08000000; // (ro)
pub const IG4_SWLTR_NSNOOP_SCALE_32US: u32 = 0x0C000000; // (ro)
pub const IG4_SWLTR_SNOOP_REQ: u32 = 0x00008000; // (rw)
pub const IG4_SWLTR_SNOOP_SCALE_MASK: u32 = 0x00001C00; // (rw)
pub const IG4_SWLTR_SNOOP_SCALE_1US: u32 = 0x00000800; // (rw)
pub const IG4_SWLTR_SNOOP_SCALE_32US: u32 = 0x00000C00; // (rw)

pub const IG4_INTR_ERR_MASK: u32 =
    IG4_INTR_TX_ABRT | IG4_INTR_TX_OVER | IG4_INTR_RX_OVER | IG4_INTR_RX_UNDER;
pub const fn ig4_param1_txfifo_depth(v: u32) -> u32 {
    ((v >> 16) & 0xff) + 1
}
pub const fn ig4_param1_rxfifo_depth(v: u32) -> u32 {
    ((v >> 8) & 0xff) + 1
}
pub const fn ig4_swltr_nsnoop_value_decode(v: u32) -> u32 {
    (v >> 16) & 0x3f
}
pub const fn ig4_swltr_nsnoop_value_encode(v: u32) -> u32 {
    (v & 0x3f) << 16
}
pub const fn ig4_swltr_snoop_value_decode(v: u32) -> u32 {
    v & 0x3f
}
pub const fn ig4_swltr_snoop_value_encode(v: u32) -> u32 {
    v & 0x3f
}
