use axerrno::{AxError, AxResult};

const MEMBARRIER_CMD_QUERY: i32 = 0;

pub fn sys_membarrier(cmd: i32, flags: u32, _cpu_id: i32) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    if cmd == MEMBARRIER_CMD_QUERY {
        Ok(0)
    } else {
        Err(AxError::InvalidInput)
    }
}
