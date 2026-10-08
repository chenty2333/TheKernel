//! Intel iwx wireless driver components.
//!
//! The implementation is based on OpenBSD's ISC-licensed iwx driver. The
//! framework-facing portions are mapped to TheKernel driver interfaces.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod alive;
mod apm;
mod ba;
mod binding;
mod bringup;
mod channel;
mod command;
mod config;
mod context;
mod control;
mod diagnostics;
mod dma;
mod firmware;
mod firmware_bundle;
mod init_cmd;
mod interrupts;
mod intr;
mod keys;
mod lifecycle;
mod mac;
mod mac_context;
mod nic;
mod notif;
mod nvm;
mod phy;
mod power;
mod queue;
mod rate;
mod registers;
mod rings;
mod rx;
mod rx_buffer;
mod rx_event;
mod rx_packet;
mod scan;
mod scan_probe;
mod spectrum;
mod startup;
mod state;
mod station;
mod statistics;
mod task;
mod tx;
mod tx_completion;

pub use alive::{
    ALIVE_STATUS_OK, ALIVE_V4_BYTES, ALIVE_V5_BYTES, ALIVE_V6_BYTES, AliveError, AliveInfo,
    parse_alive,
};
pub use apm::{
    ApmError, apm_init, apm_stop, clear_persistence_bit, force_power_gating, prepare_card_hw,
    set_hw_ready, software_reset, start_hardware,
};
pub use ba::{
    BaError, BaTimeoutAction, BarFrameRelease, INVALID_BAID, MAX_RX_BA_SESSIONS,
    RX_REORDER_TIMEOUT_MQ_USEC, ReorderBuffer, RxBaSession, RxBaTable, STATION_ID,
    baid_config_command, baid_config_response, station_ba_command, station_ba_response,
};
pub use binding::{
    BINDING_CONTEXT_COMMAND, BindingError, BindingState, BindingUpdateError, CONTEXT_ACTION_ADD,
    CONTEXT_ACTION_REMOVE, INVALID_CONTEXT_ID, LMAC_5_GHZ, LMAC_24_GHZ, MAX_MACS_IN_BINDING,
    binding_command, update_binding,
};
pub use bringup::{
    FIRMWARE_ALIVE_TIMEOUT_NS, FW_COMMAND_VERSION_UNKNOWN, FirmwareLoadError, InitFirmwareError,
    InitFirmwareState, LONG_GROUP as FIRMWARE_LONG_GROUP, PostAliveState, TX_COMMAND_OPCODE,
    UcodeStartError, load_firmware, load_ucode_wait_alive, post_alive, rate_n_flags_version,
    run_init_mvm,
};
pub use channel::{
    CHAN_2GHZ, CHAN_40MHZ, CHAN_A, CHAN_CCK, CHAN_DYN, CHAN_HT, CHAN_OFDM, CHAN_PASSIVE, CHAN_VHT,
    CHANNELS_2GHZ, CHANNELS_5GHZ, CHANNELS_24_5GHZ, CHANX_80MHZ, CHANX_160MHZ, ChannelInfo,
    VHT_CTRL_1_ABOVE, VHT_CTRL_1_BELOW, VHT_CTRL_2_ABOVE, VHT_CTRL_2_BELOW, VHT_CTRL_3_ABOVE,
    VHT_CTRL_3_BELOW, VHT_CTRL_4_ABOVE, VHT_CTRL_4_BELOW, init_channel_map, vht_control_position,
};
pub use command::{
    CMD_ASYNC, CMD_FAILED_MASK, CMD_SEND_DURING_RFKILL, CMD_WANT_RESPONSE, CommandError,
    CommandSlots, CommandTicket, CompletedCommand, EncodedCommand, HostCommand, WaitCommandError,
    command_group_id, command_opcode, command_response_status, command_version, send_host_command,
    submit_command, wait_for_command,
};
pub use config::{
    AX211_DEVICE_ID, DeviceConfig, FirmwareConfig, INTEL_VENDOR_ID, RuntimeConfig, lookup_config,
    matches_pci_device,
};
pub use context::{
    ContextError, ContextQueueAddresses, build_gen2_context, build_gen3_context,
    build_gen3_prph_scratch, set_gen3_pnvm, start_gen2_context, start_gen3_context,
};
pub use control::{
    IoctlAction, IoctlError, IoctlKind, NetworkIoctlResult, WatchdogAction, WatchdogOutcome,
    media_change, process_ioctl, watchdog_tick,
};
pub use diagnostics::{
    BZ_ERROR_TABLE_MIN, DriverDebugStatus, ERROR_ELEMENT_SIZE_BYTES, ERROR_START_OFFSET_BYTES,
    ErrorLogError, FW_SYSASSERT_CPU_MASK, LEGACY_ERROR_TABLE_MIN, LMAC_ERROR_WORDS, TxRingDebug,
    UMAC_ERROR_WORDS, driver_status, error_description, error_log_validity, parse_lmac_error_table,
    parse_umac_error_table, validate_error_table_address,
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
    DATA_PATH_GROUP, DQA_ENABLE_CMD, INIT_EXTENDED_CFG_CMD, INIT_NVM, LTR_CFG_FEATURE_ENABLE,
    LTR_CONFIG_COMMAND, LTR_VALID_STATES, NVM_ACCESS_COMPLETE_CMD, PHY_CONFIGURATION_CMD,
    REGULATORY_AND_NVM_GROUP as INIT_NVM_GROUP, SYSTEM_GROUP, TX_ANT_CONFIGURATION_CMD,
    dqa_enable_command, init_extended_config_command, ltr_config_command,
    nvm_access_complete_command, phy_configuration_command, tx_antenna_command,
};
pub use interrupts::{
    InterruptMasks, configure_msix_hardware, disable_interrupts, enable_firmware_load_interrupts,
    enable_interrupts, enable_rfkill_interrupts, hardware_rfkill, initialize_msix_hardware,
    start_firmware,
};
pub use intr::{
    ICT_ADDRESS_SHIFT, ICT_ENTRY_COUNT, ICT_SIZE_BYTES, IctError, InterruptCauseTable,
    LegacyInterruptWork, MsixInterruptWork, plan_legacy_interrupt, plan_msix_interrupt, reset_ict,
    service_legacy_interrupt, service_msix_interrupt,
};
pub use keys::{
    ADD_STA_KEY_COMMAND, ADD_STA_STATUS_MASK, ADD_STA_SUCCESS,
    DATA_PATH_GROUP as KEY_DATA_PATH_GROUP, KeyCipher, KeyConfig, KeyError, KeyTracking,
    NODE_HAVE_GROUP_KEY, NODE_HAVE_INTEGRITY_GROUP_KEY, NODE_HAVE_PAIRWISE_KEY,
    SEC_KEY_CIPHER_CCMP, SEC_KEY_COMMAND, SEC_KEY_FLAG_MCAST, SEC_KEY_FLAG_MFP,
    SETKEY_QUEUE_CAPACITY, STA_KEY_FLAG_CCM, STA_KEY_FLAG_KEY_ID_MASK, STA_KEY_FLAG_KEY_ID_SHIFT,
    STA_KEY_FLAG_MFP, STA_KEY_FLAG_MULTICAST, STA_KEY_FLAG_WEP_KEY_MAP, STA_KEY_NOT_VALID,
    SetKeyDecision, SetKeyQueue, delete_key_command, drain_key_install_queue, igtk_command,
    key_install_succeeded, legacy_delete_key_command, legacy_station_key_command,
    mld_station_key_command, set_key, station_key_command, validate_legacy_key_response,
};
pub use lifecycle::{
    InitAction, InitOutcome, InterfaceInitState, InterfaceStopState, MCAST_FILTER_COMMAND,
    MCAST_FILTER_PAYLOAD_BYTES, StopAction, allow_multicast_command, initialize_interface,
    stop_interface,
};
pub use mac::{MacAddress, flip_hardware_address, is_valid_mac_address, select_csr_mac_address};
pub use mac_context::{
    ACTION_ADD as MAC_ACTION_ADD, ACTION_REMOVE as MAC_ACTION_REMOVE, EdcaParameters, HtProtection,
    MAC_CONF_GROUP, MAC_CONFIG_COMMAND, MAC_CONTEXT_COMMAND, MacContextConfig, MacContextError,
    MacContextUpdateError, OperationMode, mac_context_command, mld_mac_context_command,
    update_mac_context,
};
pub use nic::{configure_nic, disable_rx_dma, initialize_nic, initialize_rx};
pub use notif::{
    HBUS_TARG_WRPTR, HBUS_WRPTR_RX_Q0, NotificationRingError, RFH_Q0_FRBDCB_WIDX_TRG,
    RxNotificationBatch, drain_rx_notifications,
};
pub use nvm::{
    NVM_CHANNEL_40MHZ, NVM_CHANNEL_80MHZ, NVM_CHANNEL_160MHZ, NVM_CHANNEL_ACTIVE,
    NVM_CHANNEL_VALID, NVM_GET_INFO_CMD, NVM_V3_CHANNEL_COUNT, NVM_V3_RESPONSE_BYTES,
    NVM_V4_CHANNEL_COUNT, NVM_V4_RESPONSE_BYTES, NvmError, NvmFetchError, NvmInfo,
    REGULATORY_AND_NVM_GROUP, nvm_get_command, parse_nvm_response, request_nvm_info,
};
pub use phy::{
    PHY_BAND_5GHZ, PHY_BAND_24GHZ, PHY_CONTEXT_ACTION_ADD, PHY_CONTEXT_ACTION_MODIFY,
    PHY_CONTEXT_ACTION_REMOVE, PHY_CONTEXT_COMMAND, PHY_RX_CHAIN_COUNT_SHIFT,
    PHY_RX_CHAIN_MIMO_COUNT_SHIFT, PHY_RX_CHAIN_VALID_SHIFT, PHY_WIDTH_20, PHY_WIDTH_40,
    PHY_WIDTH_80, PHY_WIDTH_160, PhyContextConfig, PhyContextError, PhyUpdateError, PhyUpdateStage,
    RLC_CONFIG_COMMAND, RLC_CONFIG_VERSION, phy_context_command, rlc_config_command,
    update_phy_context,
};
pub use power::{
    BEACON_FILTER_COMMAND, BEACON_FILTER_CONFIG_BYTES, BeaconFilterError, BeaconFilterState,
    MAC_PM_POWER_TABLE_COMMAND, POWER_ADVANCE_PM_ENABLE, POWER_KEEP_ALIVE_PERIOD_SEC,
    POWER_MANAGEMENT_ENABLE, POWER_SAVE_ENABLE, POWER_SKIP_DTIM, POWER_TABLE_COMMAND,
    POWER_UAPSD_MISBEHAVING_ENABLE, PowerApplyError, PowerCommands, PowerConfig, PowerError,
    UAPSD_RX_DATA_TIMEOUT, UAPSD_TX_DATA_TIMEOUT, WMM_AC_BE, WMM_AC_BK, WMM_AC_MASK, WMM_AC_VI,
    WMM_AC_VO, WMM_SP_2, WMM_SP_4, WMM_SP_6, WMM_SP_ALL, WMM_SP_MASK, apply_power_commands,
    beacon_filter_command, build_power_commands, set_beacon_filter, uapsd_ac_flags, uapsd_ac_mask,
    uapsd_qndp_tid, uapsd_service_period, update_beacon_abort,
};
pub use queue::{
    DATA_PATH_GROUP as TX_DATA_PATH_GROUP, DEFAULT_QUEUE_SIZE, DQA_QUEUE_ADD, DQA_QUEUE_REMOVE,
    QueueConfig, QueueError, SCD_QUEUE_CONFIG_CMD, TX_QUEUE_CFG_ENABLE_QUEUE, TxQueueError,
    TxQueueState, disable_tx_queue, dqa_queue_command, enable_tx_queue, legacy_queue_command,
    queue_cb_size, scheduler_queue_command, validate_enable_response,
};
pub use rate::{
    HtRateSet, MCS_TO_RATE_INDEX, PeerTxRateState, RATES, Rate, TLC_CHAIN_A, TLC_CHAIN_B,
    TLC_CONFIG_COMMAND, TLC_CONFIG_GROUP, TLC_FLAG_STBC, TLC_MODE_HT, TLC_MODE_NON_HT,
    TLC_MODE_VHT, TLC_RATE_STATION_ID, TLC_RATE_UPDATE_FLAG, TLC_SGI_20, TLC_SGI_40, TLC_SGI_80,
    TLC_SGI_160, TLC_UPDATE_NOTIFICATION, TLC_WIDTH_20, TLC_WIDTH_40, TLC_WIDTH_80, TLC_WIDTH_160,
    TX_FLAG_COMMAND_RATE, TX_FLAG_HIGH_PRIORITY, TlRateConfig, TlRateConfigError, TxRateError,
    TxRateInput, TxRateSelection, ack_rate_masks, apply_tlc_rate_update, fw_rate_index_cck,
    fw_rate_index_ofdm, init_rate_command, legacy_rate_index, rate_index_to_peer_rate,
    rate_value_to_index, rateset_11g_index, rateset_ht_bitmap, rateset_vht_bitmap, select_tx_rate,
    tlc_rate_command_v3, tlc_rate_command_v4,
};
pub use registers::{CsrAccess, DeviceFamily, IoBarrier, IwxRegisters, RegisterError};
pub use rings::{
    GEN3_MAX_TFD_QUEUE_SIZE, RingError, RxCompletion, RxRing, TxRing, TxSegment, allocate_rx_ring,
    allocate_tx_ring, allocate_tx_ring_for, allocate_tx_ring_for_family, tx_byte_count_entry,
};
pub use rx::{
    RX_PHY_INFO_BYTES, RxMetadataError, RxPhyInfo, noise_dbm, parse_rx_phy_info,
    signal_strength_dbm,
};
pub use rx_buffer::{
    RX_BUFFER_SIZE, RX_MPDU_COMMAND, RX_PACKET_MINIMUM_BYTES, RxBufferError, RxBufferReport,
    RxMbufPlan, process_rx_buffer, rx_buffer_packet_error,
};
pub use rx_event::{
    DriverFirmwareEvent, EventPolicyAction, EventPolicyError, FirmwareEvent, FirmwarePolicyState,
    apply_event_policy, decode_driver_event, decode_firmware_event, process_command_response,
};
pub use rx_packet::{
    FH_FRAME_ALIGNMENT, FH_FRAME_INVALID, FH_FRAME_SIZE_MASK, NOTIFICATION_ORIGIN,
    RX_PACKET_HEADER_BYTES, RxPacket, RxPacketError, parse_rx_packet,
};
pub use scan::{
    LONG_GROUP as IWX_LONG_GROUP, REDUCED_SCAN_CONFIG_API, SCAN_BAND_5GHZ, SCAN_BAND_24GHZ,
    SCAN_BAND_FLAG_SHIFT, SCAN_CONFIG_COMMAND, SCAN_ENABLE_CHANNEL_ORDER, SCAN_GEN_ADAPTIVE_DWELL,
    SCAN_GEN_FORCE_PASSIVE, SCAN_GEN_NOTIFY_ITER_COMPLETE, SCAN_GEN_PASS_ALL, SCAN_MAX_CHANNELS,
    SCAN_PASSIVE_MAX_PSD, SCAN_PRIORITY_EXT_6, ScanChannelConfig, ScanChannelConfigV5,
    ScanConfigError, ScanError, ScanState, UMAC_SCAN_ABORT, UMAC_SCAN_COMPLETE, UMAC_SCAN_REQ,
    UmacScanConfig, UmacScanError, UmacScanVersion, abort_scan, begin_background_scan,
    begin_foreground_scan, build_umac_scan_request, end_scan, fill_umac_scan_channels,
    fill_umac_scan_channels_v5, initiate_scan_command, reduced_scan_config_command,
    scan_abort_command,
};
pub use scan_probe::{
    DS_PARAMETER_IE, EXTENDED_RATES_IE, HT_CAPABILITIES_IE, PROBE_REQUEST_BYTES,
    PROBE_REQUEST_WIRE_BYTES, ProbeRequestConfig, ProbeRequestError, ProbeSegment,
    SUPPORTED_RATES_IE, ScanProbeRequest, VHT_CAPABILITIES_IE, build_scan_probe_request,
    encode_scan_probe_request,
};
pub use spectrum::{
    SF_CONFIG_BYTES, SF_CONFIG_COMMAND, SF_FULL_ON, SF_INIT_OFF, SF_LONG_DELAY_AGING,
    SF_LONG_DELAY_ON, SF_SCENARIO_COUNT, SF_TIMEOUT_TYPE_COUNT, SF_UNINIT, SF_WATERMARK_LEGACY,
    SF_WATERMARK_MIMO2, SF_WATERMARK_SCAN, SF_WATERMARK_SISO, SpectrumError, fill_spectrum_payload,
    spectrum_config_command,
};
pub use startup::{
    BT_COEX_WIFI, BT_CONFIG_COMMAND, InitHardwareAction, InitHardwareConfig, InitHardwareOutcome,
    MCC_COMMAND_BYTES, MCC_SOURCE_GET_CURRENT, MCC_SOURCE_OLD_FW, MCC_UPDATE_COMMAND,
    MCC_UPDATE_RESPONSE_MAX_VERSION, MCC_V4_HEADER_BYTES, MccUpdateResponse, PHY_OPS_GROUP,
    SOC_CONFIGURATION_COMMAND, SOC_FLAG_DISCRETE, SOC_FLAG_LOW_LATENCY, SOC_LTR_DELAY_MASK,
    SYSTEM_GROUP as STARTUP_SYSTEM_GROUP, SocConfig, StartupError, TEMP_REPORT_COMMAND_BYTES,
    TEMP_REPORTING_THRESHOLDS_COMMAND, bt_init_command, initialize_hardware, mcc_update_command,
    parse_mcc_update_response, soc_configuration_command, temperature_threshold_command,
};
pub use state::{
    AssociationState, AssociationStep, AuthError, AuthRequest, RunRequest, authenticate,
    deauthenticate, run_association, stop_association,
};
pub use station::{
    ADD_STA_COMMAND, FlushedQueue, REMOVE_STA_COMMAND, STA_FLAG_AGG_DENSITY_MASK,
    STA_FLAG_AGG_DENSITY_SHIFT, STA_FLAG_FAT_MASK, STA_FLAG_FAT_SHIFT, STA_FLAG_MAX_AGG_SIZE_MASK,
    STA_FLAG_MAX_AGG_SIZE_SHIFT, STA_FLAG_MIMO_MASK, STA_FLAG_MIMO_SHIFT, STA_FLG_DRAIN_FLOW,
    STA_ID_LINK, STA_ID_MONITOR, STA_MODE_MODIFY, STA_MODIFY_ADD_BA_TID, STA_MODIFY_UAPSD_ACS,
    STA_TYPE_GENERAL_PURPOSE, STA_TYPE_LINK, StationAddConfig, StationError, StationQueue,
    StationRemoveError, StationRemoveState, TX_FLUSH_QUEUE_INFO_BYTES, TX_FLUSH_QUEUE_LIMIT,
    TX_FLUSH_RESPONSE_BYTES, TX_PATH_FLUSH_COMMAND, TxFlushResponse, drain_station_command,
    flush_station, parse_tx_flush_response, remove_station_command, station_add_command,
    tx_path_flush_command, validate_station_add_status,
};
pub use statistics::{
    SYSTEM_GROUP as STATISTICS_SYSTEM_GROUP, SYSTEM_STATISTICS_COMMAND,
    SYSTEM_STATISTICS_END_NOTIFICATION, SystemStatisticsWait, begin_system_statistics_clear,
    legacy_statistics_clear_command, statistics_clear_command, system_statistics_clear_command,
    system_statistics_end_notification, wait_system_statistics_clear,
};
pub use task::{TaskReferences, add_task, delete_task, release_task_reference};
pub use tx::{EncodedTxFrame, TxError, TxFrame, encode_tx_frame, submit_tx_frame};
pub use tx_completion::{
    COMPRESSED_BA_HEADER_BYTES, COMPRESSED_BA_RATID_BYTES, COMPRESSED_BA_TFD_BYTES,
    CompressedBaNotification, CompressedBaTfd, TX_RESPONSE_HEADER_BYTES, TX_STATUS_DIRECT_DONE,
    TX_STATUS_MASK, TX_STATUS_SUCCESS, TxCompletionError, TxStatusNotification,
    parse_compressed_ba, parse_tx_status,
};
