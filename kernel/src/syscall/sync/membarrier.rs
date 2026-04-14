use core::sync::atomic::{Ordering, compiler_fence};

use axerrno::{AxError, AxResult};

use crate::task::AsThread;

/// Memory barrier commands
const MEMBARRIER_CMD_QUERY: i32 = 0;
const MEMBARRIER_CMD_GLOBAL: i32 = 1 << 0;
const MEMBARRIER_CMD_GLOBAL_EXPEDITED: i32 = 1 << 1;
const MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED: i32 = 1 << 2;
const MEMBARRIER_CMD_PRIVATE_EXPEDITED: i32 = 1 << 3;
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: i32 = 1 << 4;
const MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE: i32 = 1 << 5;
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE: i32 = 1 << 6;

/// Supported command flags for query
const SUPPORTED_COMMANDS: i32 = MEMBARRIER_CMD_GLOBAL
    | MEMBARRIER_CMD_GLOBAL_EXPEDITED
    | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
    | MEMBARRIER_CMD_PRIVATE_EXPEDITED
    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
    | MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE
    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE;

pub fn sys_membarrier(cmd: i32, flags: u32, _cpu_id: i32) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    match cmd {
        MEMBARRIER_CMD_QUERY => Ok(SUPPORTED_COMMANDS as isize),
        MEMBARRIER_CMD_GLOBAL | MEMBARRIER_CMD_GLOBAL_EXPEDITED => {
            compiler_fence(Ordering::SeqCst);
            Ok(0)
        }
        MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED => {
            let proc_data = axtask::current().as_thread().proc_data.clone();
            proc_data.register_membarrier(MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED as u32);
            Ok(0)
        }
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED => {
            let proc_data = axtask::current().as_thread().proc_data.clone();
            proc_data.register_membarrier(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED as u32);
            Ok(0)
        }
        MEMBARRIER_CMD_PRIVATE_EXPEDITED => {
            let proc_data = axtask::current().as_thread().proc_data.clone();
            if !proc_data.membarrier_registered(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED as u32) {
                return Err(AxError::OperationNotPermitted);
            }
            compiler_fence(Ordering::SeqCst);
            Ok(0)
        }
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE => {
            let proc_data = axtask::current().as_thread().proc_data.clone();
            proc_data
                .register_membarrier(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE as u32);
            Ok(0)
        }
        MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE => {
            let proc_data = axtask::current().as_thread().proc_data.clone();
            if !proc_data
                .membarrier_registered(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE as u32)
            {
                return Err(AxError::OperationNotPermitted);
            }
            compiler_fence(Ordering::SeqCst);
            Ok(0)
        }
        _ => Err(AxError::InvalidInput),
    }
}
