//! BPF syscall dispatcher and command handlers.

mod batch_cmd;
mod id_cmd;
mod link_cmd;
mod map_cmd;
mod obj_cmd;
mod object_cmd;
mod prog_cmd;
mod task_fd_cmd;

use axerrno::{AxError, AxResult};
use thekernel_linux_bpf::BpfCommand;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

pub use self::{
    batch_cmd::*, id_cmd::*, link_cmd::*, map_cmd::*, obj_cmd::*, object_cmd::*, prog_cmd::*,
    task_fd_cmd::*,
};

pub fn sys_bpf<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    cmd: u32,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    debug!("sys_bpf <= cmd: {cmd}, attr: {attr_ptr:#x}, size: {attr_size}");

    match BpfCommand::try_from(cmd).map_err(|_| AxError::InvalidInput)? {
        BpfCommand::MapCreate => bpf_map_create(memory, attr_ptr, attr_size),
        BpfCommand::MapLookupElem => bpf_map_lookup_elem(memory, attr_ptr, attr_size),
        BpfCommand::MapUpdateElem => bpf_map_update_elem(memory, attr_ptr, attr_size),
        BpfCommand::MapDeleteElem => bpf_map_delete_elem(memory, attr_ptr, attr_size),
        BpfCommand::MapLookupAndDeleteElem => {
            bpf_map_lookup_and_delete_elem(memory, attr_ptr, attr_size)
        }
        BpfCommand::MapGetNextKey => bpf_map_get_next_key(memory, attr_ptr, attr_size),
        BpfCommand::ProgLoad => bpf_prog_load(memory, attr_ptr, attr_size),
        BpfCommand::ProgTestRun => bpf_prog_test_run(memory, attr_ptr, attr_size),
        BpfCommand::RawTracepointOpen => bpf_raw_tracepoint_open(memory, attr_ptr, attr_size),
        BpfCommand::TaskFdQuery => bpf_task_fd_query(memory, attr_ptr, attr_size),
        BpfCommand::MapFreeze => bpf_map_freeze(memory, attr_ptr, attr_size),
        BpfCommand::LinkCreate => bpf_link_create(memory, attr_ptr, attr_size),
        BpfCommand::ProgAttach => bpf_prog_attach(memory, attr_ptr, attr_size),
        BpfCommand::ProgDetach => bpf_prog_detach(memory, attr_ptr, attr_size),
        BpfCommand::ObjPin => bpf_obj_pin(memory, attr_ptr, attr_size),
        BpfCommand::ObjGet => bpf_obj_get(memory, attr_ptr, attr_size),
        BpfCommand::ObjGetInfoByFd => bpf_obj_get_info_by_fd(memory, attr_ptr, attr_size),
        BpfCommand::BtfLoad => bpf_btf_load(memory, attr_ptr, attr_size),
        BpfCommand::BtfGetNextId => bpf_btf_get_next_id(memory, attr_ptr, attr_size),
        BpfCommand::BtfGetFdById => bpf_btf_get_fd_by_id(memory, attr_ptr, attr_size),
        BpfCommand::TokenCreate => bpf_token_create(memory, attr_ptr, attr_size),
        BpfCommand::EnableStats => bpf_enable_stats(memory, attr_ptr, attr_size),
        BpfCommand::IterCreate => bpf_iter_create(memory, attr_ptr, attr_size),
        BpfCommand::ProgQuery => bpf_prog_query(memory, attr_ptr, attr_size),
        BpfCommand::LinkUpdate => bpf_link_update(memory, attr_ptr, attr_size),
        BpfCommand::LinkGetFdById => bpf_link_get_fd_by_id(memory, attr_ptr, attr_size),
        BpfCommand::LinkGetNextId => bpf_link_get_next_id(memory, attr_ptr, attr_size),
        BpfCommand::LinkDetach => bpf_link_detach(memory, attr_ptr, attr_size),
        BpfCommand::ProgBindMap => bpf_prog_bind_map(memory, attr_ptr, attr_size),
        BpfCommand::ProgStreamReadByFd => bpf_prog_stream_read_by_fd(memory, attr_ptr, attr_size),
        BpfCommand::ProgGetNextId => bpf_prog_get_next_id(memory, attr_ptr, attr_size),
        BpfCommand::MapGetNextId => bpf_map_get_next_id(memory, attr_ptr, attr_size),
        BpfCommand::ProgGetFdById => bpf_prog_get_fd_by_id(memory, attr_ptr, attr_size),
        BpfCommand::MapGetFdById => bpf_map_get_fd_by_id(memory, attr_ptr, attr_size),
        BpfCommand::MapLookupBatch => bpf_map_lookup_batch(memory, attr_ptr, attr_size, false),
        BpfCommand::MapLookupAndDeleteBatch => {
            bpf_map_lookup_batch(memory, attr_ptr, attr_size, true)
        }
        BpfCommand::MapUpdateBatch => bpf_map_update_batch(memory, attr_ptr, attr_size),
        BpfCommand::MapDeleteBatch => bpf_map_delete_batch(memory, attr_ptr, attr_size),
    }
}
