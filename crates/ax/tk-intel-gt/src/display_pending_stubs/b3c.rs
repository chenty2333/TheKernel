// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! B3c re-exports for the translated BIOS and firmware implementations.
//! DMC, DMC-WL, DRAM and MCHBAR implementations live in their named modules.

#![allow(non_snake_case, dead_code, unsafe_code)]

pub use crate::{
    intel_dmc_upstream::{
        assert_main_dmc_loaded, intel_dmc_block_pkgc, intel_dmc_configure_dc_balance_event,
        intel_dmc_disable_pipe, intel_dmc_disable_program, intel_dmc_enable_pipe, intel_dmc_fini,
        intel_dmc_has_payload, intel_dmc_init, intel_dmc_load_program, intel_dmc_resume,
        intel_dmc_start_pkgc_exit_at_start_of_undelayed_vblank, intel_dmc_suspend,
        intel_dmc_update_dc6_allowed_count, intel_dmc_wait_fw_load, intel_pipedmc_dcb_disable,
        intel_pipedmc_dcb_enable, intel_pipedmc_disable_event, intel_pipedmc_enable_event,
        intel_pipedmc_irq_handler, intel_pipedmc_start_mmioaddr,
    },
    intel_dmc_wl_upstream::{
        intel_dmc_wl_disable, intel_dmc_wl_enable, intel_dmc_wl_flush_release_work,
        intel_dmc_wl_get, intel_dmc_wl_get_noreg, intel_dmc_wl_init, intel_dmc_wl_put,
        intel_dmc_wl_put_noreg,
    },
    intel_dram_upstream::{intel_dram_detect, intel_dram_info, intel_dram_type_str},
    intel_mchbar_upstream::{intel_mchbar_read, intel_mchbar_read16, intel_mchbar_read64_2x32},
};

pub use crate::intel_bios_upstream::{intel_bios_driver_remove, intel_bios_init};
