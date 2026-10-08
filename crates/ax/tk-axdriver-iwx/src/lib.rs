//! Intel iwx wireless driver components.
//!
//! The implementation is based on OpenBSD's ISC-licensed iwx driver. The
//! framework-facing portions are mapped to TheKernel driver interfaces.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod apm;
mod bringup;
mod command;
mod config;
mod context;
mod dma;
mod firmware;
mod firmware_bundle;
mod init_cmd;
mod interrupts;
mod mac;
mod nic;
mod nvm;
mod queue;
mod rate;
mod registers;
mod rings;
mod rx;
mod rx_packet;
mod tx;

pub use apm::{
    ApmError, apm_init, apm_stop, clear_persistence_bit, force_power_gating, prepare_card_hw,
    set_hw_ready, software_reset, start_hardware,
};
pub use bringup::{
    FIRMWARE_ALIVE_TIMEOUT_NS, FirmwareLoadError, InitFirmwareError, InitFirmwareState,
    UcodeStartError, load_firmware, load_ucode_wait_alive, run_init_mvm,
};
pub use command::{
    CMD_ASYNC, CMD_FAILED_MASK, CMD_SEND_DURING_RFKILL, CMD_WANT_RESPONSE, CommandError,
    CommandSlots, CompletedCommand, EncodedCommand, HostCommand, command_group_id, command_opcode,
    command_version, submit_command,
};
pub use config::{
    AX211_DEVICE_ID, DeviceConfig, FirmwareConfig, INTEL_VENDOR_ID, RuntimeConfig, lookup_config,
    matches_pci_device,
};
pub use context::{
    ContextError, ContextQueueAddresses, build_gen2_context, build_gen3_context,
    build_gen3_prph_scratch, set_gen3_pnvm,
};
pub use dma::{
    DebugDestinationError, DebugRegisterAccess, DebugRegisterTransaction, DmaAllocator, DmaError,
    DmaRegion, FirmwareDmaImages, LtrRegisterAccess, MonitorBuffer, PnvmDmaImage, allocate_monitor,
    allocate_monitor_block, apply_debug_destination, initialize_firmware_sections, ltr_long_value,
    set_ltr, setup_pnvm,
};
pub use firmware::{
    FirmwareError, FirmwareImage, FirmwareSection, PnvmImage, SectionType, firmware_version_string,
    is_mimo_ht_plcp, select_pnvm,
};
pub use firmware_bundle::{
    FirmwareBundle, FirmwareRequestError, request_on_rootfs_ready, take_staged,
};
pub use init_cmd::{
    DATA_PATH_GROUP, DQA_ENABLE_CMD, PHY_CONFIGURATION_CMD, TX_ANT_CONFIGURATION_CMD,
    dqa_enable_command, phy_configuration_command, tx_antenna_command,
};
pub use interrupts::{
    InterruptMasks, configure_msix_hardware, disable_interrupts, enable_firmware_load_interrupts,
    enable_interrupts, enable_rfkill_interrupts, hardware_rfkill, initialize_msix_hardware,
    start_firmware,
};
pub use mac::{MacAddress, flip_hardware_address, is_valid_mac_address, select_csr_mac_address};
pub use nic::{configure_nic, initialize_nic, initialize_rx};
pub use nvm::{
    NVM_CHANNEL_40MHZ, NVM_CHANNEL_80MHZ, NVM_CHANNEL_160MHZ, NVM_CHANNEL_ACTIVE,
    NVM_CHANNEL_VALID, NVM_GET_INFO_CMD, NVM_V3_CHANNEL_COUNT, NVM_V3_RESPONSE_BYTES,
    NVM_V4_CHANNEL_COUNT, NVM_V4_RESPONSE_BYTES, NvmError, NvmInfo, REGULATORY_AND_NVM_GROUP,
    nvm_get_command, parse_nvm_response,
};
pub use queue::{
    DATA_PATH_GROUP as TX_DATA_PATH_GROUP, DEFAULT_QUEUE_SIZE, DQA_QUEUE_ADD, DQA_QUEUE_REMOVE,
    QueueConfig, QueueError, SCD_QUEUE_CONFIG_CMD, TX_QUEUE_CFG_ENABLE_QUEUE, dqa_queue_command,
    legacy_queue_command, queue_cb_size, scheduler_queue_command, validate_enable_response,
};
pub use rate::{
    MCS_TO_RATE_INDEX, RATES, Rate, TX_FLAG_COMMAND_RATE, TX_FLAG_HIGH_PRIORITY, TxRateError,
    TxRateInput, TxRateSelection, fw_rate_index_cck, fw_rate_index_ofdm, rate_value_to_index,
    select_tx_rate,
};
pub use registers::{CsrAccess, DeviceFamily, IoBarrier, IwxRegisters, RegisterError};
pub use rings::{
    RingError, RxCompletion, RxRing, TxRing, TxSegment, allocate_rx_ring, allocate_tx_ring,
    tx_byte_count_entry,
};
pub use rx::{RxMetadataError, noise_dbm, signal_strength_dbm};
pub use rx_packet::{
    FH_FRAME_ALIGNMENT, FH_FRAME_INVALID, FH_FRAME_SIZE_MASK, NOTIFICATION_ORIGIN,
    RX_PACKET_HEADER_BYTES, RxPacket, RxPacketError, parse_rx_packet,
};
pub use tx::{EncodedTxFrame, TxError, TxFrame, encode_tx_frame, submit_tx_frame};
