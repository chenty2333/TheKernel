//! BPF program syscall command handlers: PROG_LOAD and PROG_TEST_RUN.

use alloc::{sync::Arc, vec::Vec};
use core::mem::{offset_of, size_of};

use axerrno::{AxError, AxResult};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

use crate::{
    bpf::{
        alloc_prog_id,
        defs::*,
        helpers::BpfExecution,
        prog::{BpfProgram, uses_raw_ctx_prog_type},
        read_bpf_attr, read_user_slice, register_prog_id, require_bpf_attr_range, verifier,
        write_bpf_attr_value, write_user_bytes,
    },
    bpf_security::{authorize_program_load, authorize_token_program_load, reserve_memory},
    file::{
        FileLike,
        bpf::{BpfProgFd, BpfTokenFd},
        get_typed_file,
    },
};

const BPF_PROG_TEST_RUN_MAX_TOTAL_CTX_BYTES: u64 = 4 * 1024 * 1024;
const BPF_PROG_TEST_RUN_MAX_TOTAL_INSNS: u64 = axbpf::DEFAULT_MAX_EXECUTION_STEPS as u64;
const BPF_PROG_TEST_RUN_MAX_TOTAL_AUX_BYTES: u64 = 64 * 1024 * 1024;
const BPF_PROG_LICENSE_MAX_LEN: usize = 128;

pub fn bpf_prog_load<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrProgLoad>(
        attr_size,
        offset_of!(BpfAttrProgLoad, kern_version) + size_of::<u32>(),
    )?;
    let attr: BpfAttrProgLoad = read_bpf_attr(memory, attr_ptr, attr_size)?;
    debug!(
        "bpf_prog_load: type={}, insn_cnt={}, log_level={}",
        attr.prog_type, attr.insn_cnt, attr.log_level
    );

    validate_prog_load_attr(&attr)?;
    if attr.prog_token_fd == 0 {
        authorize_program_load(attr.prog_type)?;
    } else {
        if attr.prog_token_fd < 0 {
            return Err(AxError::InvalidInput);
        }
        let token = get_typed_file::<BpfTokenFd>(attr.prog_token_fd)?;
        authorize_token_program_load(&token.grant, attr.prog_type)?;
    }

    // Validate basic parameters
    if attr.insn_cnt == 0 || attr.insn_cnt > axbpf::DEFAULT_MAX_INSTRUCTIONS as u32 {
        return Err(AxError::InvalidInput);
    }

    // Read instructions from user space
    let insns = read_user_slice(memory, attr.insns as usize, attr.insn_cnt as usize)?;

    // Read license string (for GPL check)
    let license = load_bpf_license(memory, attr.license as usize)?;
    let gpl_compatible = license_is_gpl(&license);

    // Run the verifier
    let verified = match verifier::verify_program(&insns, attr.prog_type, attr.log_level) {
        Ok(verified) => {
            write_verifier_log(memory, attr_ptr, attr_size, &attr, &verified.log)?;
            verified
        }
        Err(err) => {
            write_verifier_log(memory, attr_ptr, attr_size, &attr, &err.log)?;
            return Err(err.err);
        }
    };

    // Create program object
    let prog_id = alloc_prog_id();
    // Account for the verified instruction image and retained map bindings;
    // the verifier's bounded instruction count makes this exact and keeps the
    // charge alive with the resulting program FD.
    let instruction_bytes = (attr.insn_cnt as usize)
        .checked_mul(size_of::<BpfInsn>())
        .ok_or(AxError::NoMemory)?;
    let binding_bytes = verified
        .maps
        .len()
        .checked_mul(size_of::<crate::bpf::prog::BpfMapBinding>())
        .ok_or(AxError::NoMemory)?;
    let memory_charge = reserve_memory(
        instruction_bytes
            .checked_add(binding_bytes)
            .ok_or(AxError::NoMemory)?,
    )?;
    let program = Arc::new(BpfProgram {
        mechanism: verified.portable,
        prog_type: attr.prog_type,
        name: attr.prog_name,
        prog_id,
        expected_attach_type: attr.expected_attach_type,
        attach_btf_id: attr.attach_btf_id,
        maps: verified.maps,
        bound_maps: axsync::spin::SpinNoIrq::new(Vec::new()),
        streams: [
            axsync::Mutex::new(crate::bpf::prog::BpfStreamState::new()),
            axsync::Mutex::new(crate::bpf::prog::BpfStreamState::new()),
        ],
        gpl_compatible,
        memory_charge,
        run_time_ns: core::sync::atomic::AtomicU64::new(0),
        run_cnt: core::sync::atomic::AtomicU64::new(0),
    });
    register_prog_id(prog_id, &program)?;

    // Create fd
    BpfProgFd::new(program)
        .add_to_fd_table(false)
        .map(|fd| fd as isize)
}

pub fn bpf_prog_test_run<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrTestRun>(
        attr_size,
        offset_of!(BpfAttrTestRun, batch_size) + size_of::<u32>(),
    )?;
    let attr: BpfAttrTestRun = read_bpf_attr(memory, attr_ptr, attr_size)?;
    debug!(
        "bpf_prog_test_run: prog_fd={}, data_in={}, ctx_in={}, repeat={}",
        attr.prog_fd, attr.data_size_in, attr.ctx_size_in, attr.repeat
    );

    // Get the program from fd
    let prog_fd = BpfProgFd::from_fd(attr.prog_fd as _)?;
    let prog = &prog_fd.prog;

    let exec_insn_cnt = prog.mechanism.instructions().len();
    let repeat = validate_prog_test_run_attr(&attr, prog, exec_insn_cnt)?;

    // Read context from user space (if provided)
    let ctx_size = attr.ctx_size_in as usize;
    let ctx_template = if ctx_size > 0 {
        crate::bpf::read_user_bytes(memory, attr.ctx_in as usize, ctx_size)?
    } else {
        Vec::new()
    };
    let mut ctx = ctx_template.clone();

    // Run the program
    let mut retval = 0u64;
    let start = axhal::time::monotonic_time_nanos();
    let mut aux_budget_remaining = BPF_PROG_TEST_RUN_MAX_TOTAL_AUX_BYTES;

    for iter in 0..repeat {
        if iter != 0 && !ctx.is_empty() {
            ctx.copy_from_slice(&ctx_template);
        }
        let stats = crate::bpf::prog::BpfStatsRunGuard::begin();
        let execution = BpfExecution::new(&mut ctx, &prog.maps, aux_budget_remaining)
            .with_streams(&prog.streams);
        let result = execution.execute(&prog.mechanism);
        prog.account_run(&stats);
        (retval, aux_budget_remaining) = result?;
    }

    let duration = axhal::time::monotonic_time_nanos()
        .saturating_sub(start)
        .min(u32::MAX as u64) as u32;
    let ctx_size_out = if attr.ctx_out != 0 {
        ctx.len() as u32
    } else {
        0
    };

    write_bpf_attr_value::<BpfAttrTestRun, _, _>(
        memory,
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrTestRun, retval),
        &(retval as u32),
    )?;
    write_bpf_attr_value::<BpfAttrTestRun, _, _>(
        memory,
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrTestRun, duration),
        &duration,
    )?;

    write_bpf_attr_value::<BpfAttrTestRun, _, _>(
        memory,
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrTestRun, ctx_size_out),
        &ctx_size_out,
    )?;

    write_bpf_attr_value::<BpfAttrTestRun, _, _>(
        memory,
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrTestRun, data_size_out),
        &0u32,
    )?;

    if attr.ctx_out != 0 {
        if attr.ctx_size_out < ctx_size_out {
            return Err(AxError::StorageFull);
        }
        if !ctx.is_empty() {
            write_user_bytes(memory, attr.ctx_out as usize, &ctx)?;
        }
    }

    Ok(0)
}

fn validate_prog_load_attr(attr: &BpfAttrProgLoad) -> AxResult<()> {
    if !uses_raw_ctx_prog_type(attr.prog_type)
        && attr.prog_type != crate::bpf::prog::BPF_PROG_TYPE_LSM
    {
        return Err(AxError::InvalidInput);
    }

    if attr.kern_version != 0
        || attr.prog_flags != 0
        || (attr.expected_attach_type != 0
            && !valid_trace_expected_attach_type(attr.prog_type, attr.expected_attach_type)
            && !((attr.prog_type == crate::bpf::defs::BPF_PROG_TYPE_CGROUP_SKB
                || attr.prog_type == crate::bpf::defs::BPF_PROG_TYPE_NETFILTER
                || attr.prog_type == crate::bpf::defs::BPF_PROG_TYPE_XDP)
                && crate::bpf::prog::is_network_attach_type(attr.expected_attach_type)))
        || attr.prog_ifindex != 0
        || attr.prog_btf_fd != 0
        || attr.func_info_rec_size != 0
        || attr.func_info != 0
        || attr.func_info_cnt != 0
        || attr.line_info_rec_size != 0
        || attr.line_info != 0
        || attr.line_info_cnt != 0
        || (attr.attach_btf_id != 0
            && !matches!(
                attr.prog_type,
                crate::bpf::prog::BPF_PROG_TYPE_TRACING | crate::bpf::prog::BPF_PROG_TYPE_LSM
            ))
        || attr.attach_prog_fd_or_btf_obj_fd != 0
        || attr.core_relo_cnt != 0
        || attr.fd_array != 0
        || attr.core_relos != 0
        || attr.core_relo_rec_size != 0
        || attr.fd_array_cnt != 0
        || attr.signature != 0
        || attr.signature_size != 0
        || attr.keyring_id != 0
    {
        return Err(AxError::InvalidInput);
    }

    if attr.log_level == 0 {
        if attr.log_buf != 0 || attr.log_size != 0 {
            return Err(AxError::InvalidInput);
        }
    } else if attr.log_buf == 0 || attr.log_size == 0 {
        return Err(AxError::InvalidInput);
    }

    Ok(())
}

fn valid_trace_expected_attach_type(prog_type: u32, attach_type: u32) -> bool {
    use crate::bpf::prog::*;
    match prog_type {
        BPF_PROG_TYPE_TRACING => matches!(
            attach_type,
            BPF_TRACE_RAW_TP
                | BPF_TRACE_FENTRY
                | BPF_TRACE_FEXIT
                | BPF_MODIFY_RETURN
                | BPF_TRACE_ITER
        ),
        BPF_PROG_TYPE_LSM => attach_type == BPF_LSM_MAC,
        crate::bpf::defs::BPF_PROG_TYPE_TRACEPOINT
        | crate::bpf::defs::BPF_PROG_TYPE_RAW_TRACEPOINT
        | crate::bpf::defs::BPF_PROG_TYPE_RAW_TRACEPOINT_WRITABLE
        | crate::bpf::defs::BPF_PROG_TYPE_KPROBE => {
            attach_type == thekernel_linux_bpf::BPF_PERF_EVENT
        }
        _ => false,
    }
}

fn validate_prog_test_run_attr(
    attr: &BpfAttrTestRun,
    prog: &BpfProgram,
    insn_cnt: usize,
) -> AxResult<u32> {
    let prog_type = prog.prog_type;
    if !uses_raw_ctx_prog_type(prog_type) {
        return Err(AxError::InvalidInput);
    }

    if attr.flags != 0 || attr.cpu != 0 || attr.batch_size != 0 {
        return Err(AxError::InvalidInput);
    }
    if attr._pad0 != 0 {
        return Err(AxError::InvalidInput);
    }

    if attr.data_size_in != 0 || attr.data_size_out != 0 || attr.data_in != 0 || attr.data_out != 0
    {
        return Err(AxError::InvalidInput);
    }

    if (attr.ctx_in == 0) != (attr.ctx_size_in == 0) {
        return Err(AxError::InvalidInput);
    }
    if (attr.ctx_out == 0) != (attr.ctx_size_out == 0) {
        return Err(AxError::InvalidInput);
    }

    if matches!(
        prog_type,
        BPF_PROG_TYPE_RAW_TRACEPOINT | BPF_PROG_TYPE_RAW_TRACEPOINT_WRITABLE
    ) && (attr.ctx_out != 0 || attr.repeat != 0)
    {
        return Err(AxError::InvalidInput);
    }

    let repeat = attr.repeat.max(1);
    let total_insns = (insn_cnt as u64)
        .checked_mul(repeat as u64)
        .ok_or(AxError::InvalidInput)?;
    if total_insns > BPF_PROG_TEST_RUN_MAX_TOTAL_INSNS {
        return Err(AxError::InvalidInput);
    }

    let total_ctx_bytes = (attr.ctx_size_in as u64)
        .checked_mul(repeat as u64)
        .ok_or(AxError::InvalidInput)?;
    if total_ctx_bytes > BPF_PROG_TEST_RUN_MAX_TOTAL_CTX_BYTES {
        return Err(AxError::InvalidInput);
    }

    Ok(repeat)
}

fn license_is_gpl(license: &[u8]) -> bool {
    let s = core::str::from_utf8(license).unwrap_or("");
    s.starts_with("GPL") || s.starts_with("Dual MIT/GPL") || s.starts_with("Dual BSD/GPL")
}

fn write_verifier_log<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
    attr: &BpfAttrProgLoad,
    log: &str,
) -> AxResult<()> {
    let true_size = if attr.log_level == 0 {
        0
    } else {
        log.len().saturating_add(1).min(u32::MAX as usize) as u32
    };
    let log_true_size_end = offset_of!(BpfAttrProgLoad, log_true_size) + size_of::<u32>();
    if (attr_size as usize) >= log_true_size_end {
        write_bpf_attr_value::<BpfAttrProgLoad, _, _>(
            memory,
            attr_ptr,
            attr_size,
            offset_of!(BpfAttrProgLoad, log_true_size),
            &true_size,
        )?;
    }

    if attr.log_level == 0 {
        return Ok(());
    }

    let log_bytes = log.as_bytes();
    let copy_len = log_bytes
        .len()
        .min(attr.log_size.saturating_sub(1) as usize);
    if copy_len > 0 {
        write_user_bytes(memory, attr.log_buf as usize, &log_bytes[..copy_len])?;
    }
    let terminator = (attr.log_buf as usize)
        .checked_add(copy_len)
        .ok_or(AxError::InvalidInput)?;
    write_user_bytes(memory, terminator, &[0u8])?;

    if true_size > attr.log_size {
        return Err(AxError::StorageFull);
    }

    Ok(())
}

fn load_bpf_license<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: usize,
) -> AxResult<Vec<u8>> {
    let mut license = Vec::new();
    for index in 0..(BPF_PROG_LICENSE_MAX_LEN - 1) {
        let address = ptr.checked_add(index).ok_or(AxError::InvalidInput)?;
        let byte = crate::bpf::read_user_bytes(memory, address, 1)?[0];
        if byte == 0 {
            break;
        }
        license.push(byte);
    }
    Ok(license)
}
