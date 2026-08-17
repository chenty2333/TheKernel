//! Test-build-only control plane for I/O fault and mechanism exercises.
//!
//! Product kernels expose `/proc/io_stats` as read-only observations. This
//! module is compiled only with the explicit `test-io-control` feature and
//! keeps test policy out of that stable diagnostics surface.

use alloc::{sync::Arc, vec::Vec};
use core::str;

use axdriver::{
    AsyncBlockWaitPolicy, reset_virtio_async_block_adaptive_depth, reset_virtio_io_counters,
    set_virtio_async_block_adaptive_enabled, set_virtio_async_block_depth,
    set_virtio_async_block_enabled, set_virtio_async_block_merge_write_enabled,
    set_virtio_async_block_wait_policy, set_virtio_io_counters_enabled,
};
use axfs::{
    async_block_queue_interrupt_selftest, async_block_queue_irq_first_wait_selftest,
    async_block_queue_read_write_selftest, reset_io_stats_counters,
    set_async_dirty_flush_sg_enabled, set_cached_readahead_enabled, set_io_stats_counters_enabled,
    set_lwext4_async_mapped_read_enabled,
};
use axfs_ng_vfs::{NodePermission, VfsError, VfsResult};

use super::{RwFile, SimpleFile, SimpleFileOperation, SimpleFs};
#[cfg(feature = "asid-switch-diagnostics")]
use crate::mm::{reset_asid_switch_diagnostics, set_asid_switch_diagnostics_enabled};
#[cfg(feature = "mm-lock-diagnostics")]
use crate::mm::{reset_mm_lock_diagnostics, set_mm_lock_diagnostics_enabled};
use crate::{
    file::io_uring::{reset_io_uring_dma_direct_stats, set_io_uring_dma_direct_stats_enabled},
    mm::{
        USER_IO_PIN_TEST_DELAY_MS_MAX, reset_user_io_pin_counters,
        set_user_io_pin_counters_enabled, set_user_io_pin_test_delay_ms,
    },
};

const CONTROL_HELP: &str = concat!(
    "test-only I/O control; write exactly one key=value command\n",
    "counters=on|off|reset\n",
    "virtio_counters=on|off\n",
    "async_block=on|off\n",
    "async_block_depth=<u64>\n",
    "async_block_wait=hybrid|sync|irq_first\n",
    "async_dirty_flush_sg=on|off\n",
    "cached_readahead=on|off\n",
    "lwext4_async_read=on|off\n",
    "async_block_adaptive=on|off|reset\n",
    "async_block_merge_write=on|off\n",
    "pin_delay_ms=<0..1000>\n",
    "async_block_selftest_rw_scratch=<device>\n",
    "async_block_selftest_irq_scratch=<device>\n",
    "async_block_selftest_irq_first_scratch=<device>\n",
    "test_policy=reset\n",
);

#[cfg(feature = "mm-lock-diagnostics")]
const MM_LOCK_CONTROL_HELP: &str = "mm_lock_stats=on|off|reset\n";
#[cfg(feature = "asid-switch-diagnostics")]
const ASID_SWITCH_CONTROL_HELP: &str = "asid_switch_stats=on|off|reset\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Toggle {
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CounterCommand {
    Set(Toggle),
    Reset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdaptiveCommand {
    Set(Toggle),
    Reset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelftestKind {
    ReadWrite,
    Interrupt,
    InterruptFirst,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestIoCommand<'a> {
    Counters(CounterCommand),
    VirtioCounters(Toggle),
    AsyncBlock(Toggle),
    AsyncBlockDepth(u64),
    AsyncBlockWait(AsyncBlockWaitPolicy),
    AsyncDirtyFlushSg(Toggle),
    CachedReadahead(Toggle),
    Lwext4AsyncRead(Toggle),
    AsyncBlockAdaptive(AdaptiveCommand),
    AsyncBlockMergeWrite(Toggle),
    PinDelayMs(u64),
    #[cfg(feature = "mm-lock-diagnostics")]
    MmLockStats(CounterCommand),
    #[cfg(feature = "asid-switch-diagnostics")]
    AsidSwitchStats(CounterCommand),
    Selftest {
        kind: SelftestKind,
        scratch_device: &'a str,
    },
    ResetTestPolicy,
}

fn parse_toggle(value: &str) -> Option<Toggle> {
    match value {
        "on" => Some(Toggle::On),
        "off" => Some(Toggle::Off),
        _ => None,
    }
}

fn parse_selftest<'a>(key: &str, value: &'a str) -> Result<Option<TestIoCommand<'a>>, VfsError> {
    let kind = match key {
        "async_block_selftest_rw_scratch" => SelftestKind::ReadWrite,
        "async_block_selftest_irq_scratch" => SelftestKind::Interrupt,
        "async_block_selftest_irq_first_scratch" => SelftestKind::InterruptFirst,
        _ => return Ok(None),
    };
    if value.is_empty() || value == axfs::ROOT_BLOCK_DEVICE_NAME || value.contains('/') {
        return Err(VfsError::InvalidInput);
    }
    Ok(Some(TestIoCommand::Selftest {
        kind,
        scratch_device: value,
    }))
}

fn parse_command(text: &str) -> VfsResult<TestIoCommand<'_>> {
    let command = text.trim();
    let (key, value) = command.split_once('=').ok_or(VfsError::InvalidInput)?;
    if key.trim() != key || value.trim() != value || key.is_empty() || value.is_empty() {
        return Err(VfsError::InvalidInput);
    }

    let parsed = match key {
        "counters" => TestIoCommand::Counters(match value {
            "reset" => CounterCommand::Reset,
            _ => CounterCommand::Set(parse_toggle(value).ok_or(VfsError::InvalidInput)?),
        }),
        "virtio_counters" => {
            TestIoCommand::VirtioCounters(parse_toggle(value).ok_or(VfsError::InvalidInput)?)
        }
        "async_block" => {
            TestIoCommand::AsyncBlock(parse_toggle(value).ok_or(VfsError::InvalidInput)?)
        }
        "async_block_depth" => TestIoCommand::AsyncBlockDepth(
            value.parse::<u64>().map_err(|_| VfsError::InvalidInput)?,
        ),
        "async_block_wait" => TestIoCommand::AsyncBlockWait(match value {
            "hybrid" => AsyncBlockWaitPolicy::Hybrid,
            "sync" => AsyncBlockWaitPolicy::Sync,
            "irq_first" => AsyncBlockWaitPolicy::InterruptFirst,
            _ => return Err(VfsError::InvalidInput),
        }),
        "async_dirty_flush_sg" => {
            TestIoCommand::AsyncDirtyFlushSg(parse_toggle(value).ok_or(VfsError::InvalidInput)?)
        }
        "cached_readahead" => {
            TestIoCommand::CachedReadahead(parse_toggle(value).ok_or(VfsError::InvalidInput)?)
        }
        "lwext4_async_read" => {
            TestIoCommand::Lwext4AsyncRead(parse_toggle(value).ok_or(VfsError::InvalidInput)?)
        }
        "async_block_adaptive" => TestIoCommand::AsyncBlockAdaptive(match value {
            "reset" => AdaptiveCommand::Reset,
            _ => AdaptiveCommand::Set(parse_toggle(value).ok_or(VfsError::InvalidInput)?),
        }),
        "async_block_merge_write" => {
            TestIoCommand::AsyncBlockMergeWrite(parse_toggle(value).ok_or(VfsError::InvalidInput)?)
        }
        "pin_delay_ms" => {
            let delay_ms = value.parse::<u64>().map_err(|_| VfsError::InvalidInput)?;
            if delay_ms > USER_IO_PIN_TEST_DELAY_MS_MAX {
                return Err(VfsError::InvalidInput);
            }
            TestIoCommand::PinDelayMs(delay_ms)
        }
        #[cfg(feature = "mm-lock-diagnostics")]
        "mm_lock_stats" => TestIoCommand::MmLockStats(match value {
            "reset" => CounterCommand::Reset,
            _ => CounterCommand::Set(parse_toggle(value).ok_or(VfsError::InvalidInput)?),
        }),
        #[cfg(feature = "asid-switch-diagnostics")]
        "asid_switch_stats" => TestIoCommand::AsidSwitchStats(match value {
            "reset" => CounterCommand::Reset,
            _ => CounterCommand::Set(parse_toggle(value).ok_or(VfsError::InvalidInput)?),
        }),
        "test_policy" if value == "reset" => TestIoCommand::ResetTestPolicy,
        _ => return parse_selftest(key, value)?.ok_or(VfsError::InvalidInput),
    };
    Ok(parsed)
}

fn enabled(toggle: Toggle) -> bool {
    toggle == Toggle::On
}

fn reset_test_policy() -> VfsResult<()> {
    set_io_stats_counters_enabled(false);
    set_io_uring_dma_direct_stats_enabled(false);
    set_user_io_pin_counters_enabled(false);
    set_virtio_io_counters_enabled(false);
    set_virtio_async_block_enabled(false);
    set_virtio_async_block_adaptive_enabled(false);
    set_virtio_async_block_merge_write_enabled(false);
    set_async_dirty_flush_sg_enabled(false);
    set_cached_readahead_enabled(false);
    set_lwext4_async_mapped_read_enabled(false);
    set_user_io_pin_test_delay_ms(0).map_err(|_| VfsError::InvalidInput)?;
    #[cfg(feature = "mm-lock-diagnostics")]
    set_mm_lock_diagnostics_enabled(false).map_err(|_| VfsError::InvalidInput)?;
    #[cfg(feature = "asid-switch-diagnostics")]
    set_asid_switch_diagnostics_enabled(false);
    Ok(())
}

fn apply_command(command: TestIoCommand<'_>) -> VfsResult<()> {
    match command {
        TestIoCommand::Counters(CounterCommand::Set(toggle)) => {
            let enabled = enabled(toggle);
            set_io_stats_counters_enabled(enabled);
            set_io_uring_dma_direct_stats_enabled(enabled);
            set_user_io_pin_counters_enabled(enabled);
        }
        TestIoCommand::Counters(CounterCommand::Reset) => {
            reset_io_stats_counters();
            reset_io_uring_dma_direct_stats();
            reset_user_io_pin_counters();
            reset_virtio_io_counters();
        }
        TestIoCommand::VirtioCounters(toggle) => {
            set_virtio_io_counters_enabled(enabled(toggle));
        }
        TestIoCommand::AsyncBlock(toggle) => {
            set_virtio_async_block_enabled(enabled(toggle));
        }
        TestIoCommand::AsyncBlockDepth(depth) => set_virtio_async_block_depth(depth),
        TestIoCommand::AsyncBlockWait(policy) => set_virtio_async_block_wait_policy(policy),
        TestIoCommand::AsyncDirtyFlushSg(toggle) => {
            set_async_dirty_flush_sg_enabled(enabled(toggle));
        }
        TestIoCommand::CachedReadahead(toggle) => {
            set_cached_readahead_enabled(enabled(toggle));
        }
        TestIoCommand::Lwext4AsyncRead(toggle) => {
            set_lwext4_async_mapped_read_enabled(enabled(toggle));
        }
        TestIoCommand::AsyncBlockAdaptive(AdaptiveCommand::Set(toggle)) => {
            set_virtio_async_block_adaptive_enabled(enabled(toggle));
        }
        TestIoCommand::AsyncBlockAdaptive(AdaptiveCommand::Reset) => {
            reset_virtio_async_block_adaptive_depth();
        }
        TestIoCommand::AsyncBlockMergeWrite(toggle) => {
            set_virtio_async_block_merge_write_enabled(enabled(toggle));
        }
        TestIoCommand::PinDelayMs(delay_ms) => {
            set_user_io_pin_test_delay_ms(delay_ms).map_err(|_| VfsError::InvalidInput)?;
        }
        #[cfg(feature = "mm-lock-diagnostics")]
        TestIoCommand::MmLockStats(CounterCommand::Set(toggle)) => {
            set_mm_lock_diagnostics_enabled(enabled(toggle)).map_err(|_| VfsError::InvalidInput)?;
        }
        #[cfg(feature = "mm-lock-diagnostics")]
        TestIoCommand::MmLockStats(CounterCommand::Reset) => {
            reset_mm_lock_diagnostics().map_err(|_| VfsError::InvalidInput)?;
        }
        #[cfg(feature = "asid-switch-diagnostics")]
        TestIoCommand::AsidSwitchStats(CounterCommand::Set(toggle)) => {
            set_asid_switch_diagnostics_enabled(enabled(toggle));
        }
        #[cfg(feature = "asid-switch-diagnostics")]
        TestIoCommand::AsidSwitchStats(CounterCommand::Reset) => {
            reset_asid_switch_diagnostics();
        }
        TestIoCommand::Selftest {
            kind,
            scratch_device,
        } => {
            let result = match kind {
                SelftestKind::ReadWrite => async_block_queue_read_write_selftest(scratch_device),
                SelftestKind::Interrupt => async_block_queue_interrupt_selftest(scratch_device),
                SelftestKind::InterruptFirst => {
                    async_block_queue_irq_first_wait_selftest(scratch_device)
                }
            };
            result.map_err(|_| VfsError::InvalidInput)?;
        }
        TestIoCommand::ResetTestPolicy => reset_test_policy()?,
    }
    Ok(())
}

fn control_operation(request: SimpleFileOperation<'_>) -> VfsResult<Option<Vec<u8>>> {
    match request {
        SimpleFileOperation::Read => {
            let mut help = CONTROL_HELP.as_bytes().to_vec();
            #[cfg(feature = "mm-lock-diagnostics")]
            help.extend_from_slice(MM_LOCK_CONTROL_HELP.as_bytes());
            #[cfg(feature = "asid-switch-diagnostics")]
            help.extend_from_slice(ASID_SWITCH_CONTROL_HELP.as_bytes());
            Ok(Some(help))
        }
        SimpleFileOperation::Write(data) => {
            if data.iter().all(|byte| byte.is_ascii_whitespace()) {
                return Ok(None);
            }
            let text = str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
            apply_command(parse_command(text)?)?;
            Ok(None)
        }
    }
}

pub(super) fn new_file(fs: Arc<SimpleFs>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(
        fs,
        NodePermission::from_bits_truncate(0o600),
        RwFile::new(control_operation),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_accepts_only_canonical_assignments() {
        assert_eq!(
            parse_command("async_block_wait=irq_first"),
            Ok(TestIoCommand::AsyncBlockWait(
                AsyncBlockWaitPolicy::InterruptFirst
            ))
        );
        assert_eq!(
            parse_command("pin_delay_ms=1000"),
            Ok(TestIoCommand::PinDelayMs(1000))
        );
        assert!(parse_command("async_block_on").is_err());
        assert!(parse_command("async_block =on").is_err());
        assert!(parse_command("pin_delay_ms=1001").is_err());
        #[cfg(feature = "mm-lock-diagnostics")]
        assert_eq!(
            parse_command("mm_lock_stats=reset"),
            Ok(TestIoCommand::MmLockStats(CounterCommand::Reset))
        );
        #[cfg(feature = "asid-switch-diagnostics")]
        assert_eq!(
            parse_command("asid_switch_stats=reset"),
            Ok(TestIoCommand::AsidSwitchStats(CounterCommand::Reset))
        );
    }

    #[test]
    fn destructive_selftests_require_an_explicit_non_root_scratch_device() {
        assert_eq!(
            parse_command("async_block_selftest_rw_scratch=vdb"),
            Ok(TestIoCommand::Selftest {
                kind: SelftestKind::ReadWrite,
                scratch_device: "vdb",
            })
        );
        assert!(parse_command("async_block_selftest_rw_scratch=vda").is_err());
        assert!(parse_command("async_block_selftest_rw_scratch=/dev/vdb").is_err());
        assert!(parse_command("async_block_selftest_rw_scratch=").is_err());
        assert!(parse_command("async_block_selftest_rw").is_err());
    }

    #[test]
    fn removed_user_direct_async_control_is_rejected() {
        assert!(parse_command("user_direct_async=on").is_err());
    }
}
