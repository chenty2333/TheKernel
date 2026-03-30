//! BPF syscall dispatcher and command handlers.

mod map_cmd;
mod obj_cmd;
mod prog_cmd;

use axerrno::{AxError, AxResult};

pub use self::{map_cmd::*, obj_cmd::*, prog_cmd::*};
use crate::bpf::defs::*;

pub fn sys_bpf(cmd: u32, attr_ptr: usize, attr_size: u32) -> AxResult<isize> {
    debug!("sys_bpf <= cmd: {cmd}, attr: {attr_ptr:#x}, size: {attr_size}");

    match cmd {
        BPF_MAP_CREATE => bpf_map_create(attr_ptr, attr_size),
        BPF_MAP_LOOKUP_ELEM => bpf_map_lookup_elem(attr_ptr, attr_size),
        BPF_MAP_UPDATE_ELEM => bpf_map_update_elem(attr_ptr, attr_size),
        BPF_MAP_DELETE_ELEM => bpf_map_delete_elem(attr_ptr, attr_size),
        BPF_MAP_GET_NEXT_KEY => bpf_map_get_next_key(attr_ptr, attr_size),
        BPF_PROG_LOAD => bpf_prog_load(attr_ptr, attr_size),
        BPF_PROG_TEST_RUN => bpf_prog_test_run(attr_ptr, attr_size),
        BPF_OBJ_GET_INFO_BY_FD => bpf_obj_get_info_by_fd(attr_ptr, attr_size),
        BPF_MAP_FREEZE => bpf_map_freeze(attr_ptr, attr_size),
        _ => {
            warn!("sys_bpf: unsupported cmd {cmd}");
            Err(AxError::InvalidInput)
        }
    }
}
