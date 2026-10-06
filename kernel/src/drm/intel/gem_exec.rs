// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. Repository MIT license.
//! Original bounded N305 i915 wire adapter over existing GEM/reservation/syncobj.
//! Facts from Linux7.2.3 include/uapi/drm/i915_drm.h and gem execbuf validation;
//! no GPL body is copied. No arbitrary privileged batch reaches the engine.
use alloc::{sync::Arc, vec::Vec};
use core::{mem::size_of, time::Duration};

use axerrno::{AxError, AxResult};

use crate::{
    drm::{
        DrmFile, GemBacking,
        fence::{Fence, Reservation},
        gem::{GemMemoryCharge, GemObject},
        ioctl::{UserCopy, read_array, read_pod, write_pod},
    },
    mm::SharedPages,
};

#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Create {
    size: u64,
    handle: u32,
    pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Data {
    handle: u32,
    pad: u32,
    offset: u64,
    size: u64,
    address: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Mmap {
    handle: u32,
    pad: u32,
    offset: u64,
    flags: u64,
    extensions: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Busy {
    handle: u32,
    busy: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Wait {
    handle: u32,
    flags: u32,
    timeout_ns: i64,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct Exec {
    buffers: u64,
    count: u32,
    start: u32,
    length: u32,
    dr1: u32,
    dr4: u32,
    fence_count: u32,
    fences: u64,
    flags: u64,
    pub(super) context: u64,
    reserved: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Object {
    handle: u32,
    relocations: u32,
    relocation_pointer: u64,
    alignment: u64,
    offset: u64,
    flags: u64,
    reserved1: u64,
    reserved2: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct ExecFence {
    handle: u32,
    flags: u32,
}
pub(super) const fn command<T>(nr: u64, rw: u64) -> u32 {
    ((rw << 30) | (size_of::<T>() as u64) << 16 | (b'd' as u64) << 8 | (0x40 + nr)) as u32
}
const CREATE: u32 = command::<Create>(0x1b, 3);
const PREAD: u32 = command::<Data>(0x1c, 1);
const PWRITE: u32 = command::<Data>(0x1d, 1);
const MMAP: u32 = command::<Mmap>(0x24, 3);
const BUSY: u32 = command::<Busy>(0x17, 3);
const WAIT: u32 = command::<Wait>(0x2c, 3);
const EXEC: u32 = command::<Exec>(0x29, 1);
const EXEC_WR: u32 = command::<Exec>(0x29, 3);
// Compiled local Linux7.2.3 UAPI header comparison (x86_64); pure wire facts.
const _: () = {
    assert!(size_of::<Create>() == 16 && size_of::<Data>() == 32 && size_of::<Mmap>() == 32);
    assert!(size_of::<Busy>() == 8 && size_of::<Wait>() == 16 && size_of::<Exec>() == 64);
    assert!(size_of::<Object>() == 56 && size_of::<ExecFence>() == 8);
    assert!(CREATE == 0xc010645b && PREAD == 0x4020645c && PWRITE == 0x4020645d);
    assert!(MMAP == 0xc0206464 && BUSY == 0xc0086457 && WAIT == 0xc010646c);
    assert!(EXEC == 0x40406469 && EXEC_WR == 0xc0406469);
    assert!(core::mem::offset_of!(Exec, fences) == 32 && core::mem::offset_of!(Exec, flags) == 40);
    assert!(
        core::mem::offset_of!(Exec, context) == 48 && core::mem::offset_of!(Object, offset) == 24
    );
    assert!(core::mem::offset_of!(Object, flags) == 32);
};
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct TimelineExt {
    next: u64,
    name: u32,
    flags: u32,
    reserved: [u32; 4],
    count: u64,
    handles: u64,
    values: u64,
}
const EXTENSIONS: u64 = 1 << 21;
// Independently compiled local Linux7.2.3 header: 32-byte extension header,
// 56-byte timeline record; no implicit omission of its four reserved words.
const _: () = {
    assert!(size_of::<TimelineExt>() == 56);
    assert!(core::mem::offset_of!(TimelineExt, count) == 32);
    assert!(core::mem::offset_of!(TimelineExt, handles) == 40);
    assert!(core::mem::offset_of!(TimelineExt, values) == 48);
};

const NO_RELOC: u64 = 1 << 11;
const FENCE_ARRAY: u64 = 1 << 19;
const FENCE_IN: u64 = 1 << 16;
const FENCE_OUT: u64 = 1 << 17;
const PINNED: u64 = 1 << 4;
const ADDRESS48: u64 = 1 << 3;
const WRITE: u64 = 1 << 2;
const MAX_SIZE: u64 = 65536;
struct Backing {
    pages: Arc<SharedPages>,
    _charge: Arc<GemMemoryCharge>,
}
impl GemBacking for Backing {
    fn shared_pages(&self) -> crate::drm::DrmResult<Arc<SharedPages>> {
        Ok(self.pages.clone())
    }
}
pub(super) fn object(file: &DrmFile, handle: u32) -> AxResult<Arc<GemObject>> {
    file.gem(handle).map_err(AxError::from)
}
fn previous(object: &GemObject, timeout: Option<Duration>) -> AxResult<()> {
    if let Some(fence) = object.reservation.predecessor() {
        fence.wait(timeout)?;
    }
    Ok(())
}
fn range(object: &GemObject, offset: u64, size: u64) -> AxResult<(usize, usize)> {
    let end = offset.checked_add(size).ok_or(AxError::InvalidInput)?;
    if end > object.size || size > MAX_SIZE {
        return Err(AxError::InvalidInput);
    }
    Ok((
        usize::try_from(offset).map_err(|_| AxError::InvalidInput)?,
        usize::try_from(size).map_err(|_| AxError::InvalidInput)?,
    ))
}
fn create(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut request: Create = read_pod(copy, arg)?;
    if request.size == 0 || request.size > MAX_SIZE || request.pad != 0 {
        return Err(AxError::InvalidInput);
    }
    request.size = (request.size + 4095) & !4095;
    let charge = file
        .reserve_render_memory(request.size as usize)
        .map_err(AxError::from)?;
    let pages = Arc::try_new(SharedPages::new_fixed(
        request.size as usize,
        axhal::paging::PageSize::Size4K,
    )?)
    .map_err(|_| AxError::NoMemory)?;
    // Attach the accounting lifetime to the real pages, so mmap/PRIME views
    // still charge memory after the originating handle/backing closes.
    pages.retain_allocation_owner(charge.clone())?;
    let backing = Arc::try_new(Backing {
        pages,
        _charge: charge,
    })
    .map_err(|_| AxError::NoMemory)?;
    request.handle = file
        .create_system_gem(backing, request.size)
        .map_err(AxError::from)?;
    if let Err(error) = write_pod(copy, arg, &request) {
        let _ = file.close_handle(request.handle);
        return Err(error);
    }
    Ok(())
}
struct CpuCompletion(Arc<Fence>);
impl Drop for CpuCompletion {
    fn drop(&mut self) {
        self.0.signal();
    }
}
fn data(file: &DrmFile, copy: &impl UserCopy, arg: usize, write: bool) -> AxResult<()> {
    let request: Data = read_pod(copy, arg)?;
    if request.pad != 0 {
        return Err(AxError::InvalidInput);
    }
    let object = object(file, request.handle)?;
    let (offset, count) = range(&object, request.offset, request.size)?;
    let mut data = Vec::new();
    data.try_reserve_exact(count)
        .map_err(|_| AxError::NoMemory)?;
    data.resize(count, 0);
    let address = usize::try_from(request.address).map_err(|_| AxError::BadAddress)?;
    if write && count != 0 {
        // Snapshot all user data before admission; no partial object mutation
        // if a late usercopy page faults. Only initialized bytes are accepted.
        // SAFETY: initialized vector reserves exactly count bytes; UserCopy
        // fills them through a byte-aligned MaybeUninit view without resizing.
        let dst = unsafe { core::slice::from_raw_parts_mut(data.as_mut_ptr().cast(), count) };
        copy.read(address, dst)?;
    }
    let fence = Fence::new(false);
    if let Some(prior) = object.reservation.replace(fence.clone())
        && let Err(error) = prior.wait(Some(Duration::from_millis(500)))
    {
        fence.signal_error();
        return Err(error);
    }
    let _completion = CpuCompletion(fence);
    let pages = object.backing.shared_pages().map_err(AxError::from)?;
    if write {
        pages.write_bytes(offset, &data)?;
    } else {
        pages.read_bytes(offset, &mut data)?;
        if count != 0 {
            copy.write(address, &data)?;
        }
    }
    Ok(())
}
fn wait(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: Wait = read_pod(copy, arg)?;
    if r.flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let object = object(file, r.handle)?;
    let start = axhal::time::monotonic_time_nanos();
    let timeout = if r.timeout_ns < 0 {
        None
    } else {
        Some(Duration::from_nanos(r.timeout_ns as u64))
    };
    let result = previous(&object, timeout);
    if r.timeout_ns >= 0 {
        r.timeout_ns = r
            .timeout_ns
            .saturating_sub(
                axhal::time::monotonic_time_nanos()
                    .saturating_sub(start)
                    .min(i64::MAX as u64) as i64,
            )
            .max(0);
    }
    write_pod(copy, arg, &r)?;
    result.map_err(|error| {
        if error == AxError::WouldBlock {
            AxError::TimedOut
        } else {
            error
        }
    })
}
#[derive(Clone, Copy)]
pub(super) enum Plan {
    Copy(intel_gt::bcs::Copy),
    Render,
}
fn decode(
    file: &[Arc<GemObject>],
    pages: &[Arc<SharedPages>],
    start: usize,
    render: bool,
) -> AxResult<Plan> {
    if render {
        if start != 0 || file[0].size < 24576 || file[1].size < 24576 || file[2].size < 4096 {
            return Err(AxError::InvalidInput);
        }
        let mut bytes = [0u8; 4096];
        pages[2].read_bytes(0, &mut bytes)?;
        for (i, expected) in intel_gt::rcs_page::PAGE.iter().enumerate() {
            if u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap()) != *expected {
                return Err(AxError::InvalidInput);
            }
        }
        Ok(Plan::Render)
    } else {
        let mut bytes = [0u8; 44];
        pages[2].read_bytes(start, &mut bytes)?;
        let words: [u32; 11] = core::array::from_fn(|i| {
            u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
        });
        intel_gt::bcs::decode_copy(&words, file[0].size, file[1].size)
            .map(Plan::Copy)
            .map_err(|_| AxError::InvalidInput)
    }
}
/// Snapshot and validate first, publish shared completion edges atomically,
/// then execute over pinned views. Native implementations never execute the
/// user's memory as commands, including a concurrent writable batch mapping.
pub(super) fn exec_with(
    file: &DrmFile,
    copy: &impl UserCopy,
    arg: usize,
    execute: impl FnOnce(Arc<SharedPages>, Arc<SharedPages>, Plan) -> AxResult<()>,
) -> AxResult<()> {
    let r: Exec = read_pod(copy, arg)?;
    // The memory-only helper cannot resolve process descriptors.
    if r.flags & (FENCE_IN | FENCE_OUT) != 0 {
        return Err(AxError::InvalidInput);
    }
    exec_request(file, copy, r, Fence::new(false), None, |s, d, p, _vm| {
        execute(s, d, p)
    })
}
pub(super) fn exec_request(
    file: &DrmFile,
    copy: &impl UserCopy,
    r: Exec,
    completion: Arc<Fence>,
    input: Option<Arc<Fence>>,
    execute: impl FnOnce(
        Arc<SharedPages>,
        Arc<SharedPages>,
        Plan,
        Arc<super::gt::copy::Vm>,
    ) -> AxResult<()>,
) -> AxResult<()> {
    let render = r.flags & 0x3f == 1;
    if r.count != 3
        || r.length
            != if render {
                intel_gt::rcs_page::COMMAND_BYTES
            } else {
                44
            }
        || !r.start.is_multiple_of(8)
        || r.dr1 != 0
        || r.dr4 != 0
        || r.context >> 32 != 0
        || (r.flags & (FENCE_IN | FENCE_OUT) == 0 && r.reserved != 0)
        || (r.flags & FENCE_IN != 0) != input.is_some()
        || r.flags & !(FENCE_ARRAY | EXTENSIONS | FENCE_IN | FENCE_OUT)
            != (if render { 1 } else { 3 } | NO_RELOC)
        || r.fence_count > 64
        || r.flags & (FENCE_ARRAY | EXTENSIONS) == (FENCE_ARRAY | EXTENSIONS)
        || (r.flags & FENCE_ARRAY == 0 && r.fence_count != 0)
        || (r.flags & (FENCE_ARRAY | EXTENSIONS) == 0 && r.fences != 0)
    {
        return Err(AxError::InvalidInput);
    }
    // Lookup pins the old context through a concurrent destroy. The per-
    // context sleepable gate orders synchronous jobs without a spinlock wait.
    let context = file.intel_contexts.lookup(r.context as u32)?;
    let mut context_job = context.lock();
    let records = read_array::<Object>(copy, r.buffers, 3, 3)?;
    let mut objects = Vec::new();
    let mut pages = Vec::new();
    objects
        .try_reserve_exact(3)
        .map_err(|_| AxError::NoMemory)?;
    pages.try_reserve_exact(3).map_err(|_| AxError::NoMemory)?;
    for (i, o) in records.iter().enumerate() {
        let permitted = PINNED | ADDRESS48 | if i == 1 { WRITE } else { 0 };
        if o.relocations != 0
            || o.relocation_pointer != 0
            || !matches!(o.alignment, 0 | 4096)
            || o.offset != 0x10000 * (i as u64 + 1)
            || o.flags & !(ADDRESS48 | WRITE) != PINNED
            || o.flags & !permitted != 0
            || (i == 1 && o.flags & WRITE == 0)
            || o.reserved1 != 0
            || o.reserved2 != 0
        {
            return Err(AxError::InvalidInput);
        }
        let object = object(file, o.handle)?;
        if object.size == 0 || object.size > MAX_SIZE || object.backing.host_resource().is_some() {
            return Err(AxError::InvalidInput);
        }
        let ram = object.backing.shared_pages().map_err(AxError::from)?;
        if pages.iter().any(|p| Arc::ptr_eq(p, &ram)) {
            return Err(AxError::InvalidInput);
        }
        objects.push(object);
        pages.push(ram);
    }
    let (fences, points) = if r.flags & FENCE_ARRAY != 0 {
        let fences = read_array::<ExecFence>(copy, r.fences, r.fence_count as usize, 64)?;
        let mut points = Vec::new();
        points
            .try_reserve_exact(fences.len())
            .map_err(|_| AxError::NoMemory)?;
        points.resize(fences.len(), 0);
        (fences, points)
    } else if r.flags & EXTENSIONS != 0 && r.fences != 0 {
        // One source timeline extension, not a generic extension interpreter.
        let ext: TimelineExt = read_pod(
            copy,
            usize::try_from(r.fences).map_err(|_| AxError::BadAddress)?,
        )?;
        if ext.next != 0
            || ext.name != 0
            || ext.flags != 0
            || ext.reserved != [0; 4]
            || ext.count > 64
        {
            return Err(AxError::InvalidInput);
        }
        (
            read_array::<ExecFence>(copy, ext.handles, ext.count as usize, 64)?,
            read_array::<u64>(copy, ext.values, ext.count as usize, 64)?,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    inputs
        .try_reserve_exact(fences.len() + usize::from(input.is_some()))
        .map_err(|_| AxError::NoMemory)?;
    if let Some(input) = input {
        inputs.push(input);
    }
    outputs
        .try_reserve_exact(fences.len())
        .map_err(|_| AxError::NoMemory)?;
    for (f, point) in fences.into_iter().zip(points) {
        if f.flags & !3 != 0 || (point != 0 && f.flags == 3) {
            return Err(AxError::InvalidInput);
        }
        let object = file.syncobj(f.handle).map_err(AxError::from)?;
        if f.flags & 1 != 0 {
            // The source snapshots an already materialized input fence;
            // missing points are EINVAL, not a fabricated successful wait.
            inputs.push(object.fence_at(point).map_err(|_| AxError::InvalidInput)?);
        }
        if f.flags & 2 != 0 {
            if point != 0
                && (point <= object.query_point(true)
                    || outputs
                        .iter()
                        .any(|(other, p)| Arc::ptr_eq(other, &object) && *p == point))
            {
                return Err(AxError::InvalidInput);
            }
            outputs.push((object, point));
        }
    }
    // Explicit producers may be writing the batch too. Wait their original
    // captured fence identities BEFORE reading the command snapshot.
    for prior in &inputs {
        prior.wait(Some(Duration::from_millis(500)))?;
    }
    let (start, _) = range(
        &objects[2],
        u64::from(r.start),
        if render { 4096 } else { 44 },
    )?;
    previous(&objects[2], Some(Duration::from_millis(500)))?;
    let _admission = decode(&objects, &pages, start, render)?;
    let vm = context_job.begin(file)?;
    // Capture/wait producers before taking a shared VM execution gate: their
    // jobs may need the same root through another context.
    let _vm_job = vm.gate.lock();
    let mut refs = [
        &objects[0].reservation,
        &objects[1].reservation,
        &objects[2].reservation,
    ];
    let predecessors = match Reservation::replace_many(&mut refs, completion.clone()) {
        Ok(p) => p,
        Err(e) => {
            completion.signal_error();
            return Err(e);
        }
    };
    let result = (|| {
        // Publication may allocate a consumer chain or race another producer.
        // Every post-reservation failure reaches the same terminal error below.
        for (out, point) in outputs {
            out.submit_point(point, completion.clone())?;
        }
        for prior in &predecessors {
            prior.wait(Some(Duration::from_millis(500)))?;
        }
        // A CPU pwrite could have been admitted between the first snapshot
        // and atomic reservation publication. Observe its final contents now;
        // later pwrite is behind our completion. Mmap races cannot inject GPU
        // commands: the native adapter rebuilds this bounded decoded plan.
        let plan = decode(&objects, &pages, start, render)?;
        execute(pages[0].clone(), pages[1].clone(), plan, vm.clone())
    })();
    if result.is_ok() {
        completion.signal();
    } else {
        completion.signal_error();
    }
    result
}
/// Use the caller's captured files table, never a worker's ambient table.
/// Reserve and prepare the output before executing; publication is infallible
/// and happens only after successful execution and result copyout.
fn prepare_output(
    table: Arc<crate::file::FdTable>,
    limit: usize,
    completion: Arc<Fence>,
) -> AxResult<crate::file::PreparedFdPublication> {
    let slot = crate::file::reserve_fd_in(table, limit, true)?;
    let description =
        crate::file::FileDescription::new(crate::drm::syncobj::SyncFile::new(completion))?;
    slot.prepare_publication(description)
}
fn finish_output(
    copy: &impl UserCopy,
    arg: usize,
    mut request: Exec,
    output: Option<crate::file::PreparedFdPublication>,
) -> AxResult<()> {
    if let Some(output) = output {
        request.reserved = (request.reserved & 0xffff_ffff) | ((output.fd() as u64) << 32);
        // Failed copyout drops only the unpublished slot. Completed GPU work
        // and its GEM/syncobj fences remain terminal; do not undo execution.
        write_pod(copy, arg, &request)?;
        output.commit();
    }
    Ok(())
}
pub(crate) fn dispatch_native(
    file: &DrmFile,
    context: &crate::file::IoctlContext,
    cmd: u32,
    arg: usize,
) -> AxResult<usize> {
    if !matches!(cmd, EXEC | EXEC_WR) {
        return dispatch(file, context, cmd, arg);
    }
    let request: Exec = read_pod(context, arg)?;
    // Refuse the source's documented write-only OUT-fd leak hazard.
    if request.flags & FENCE_OUT != 0 && cmd != EXEC_WR {
        return Err(AxError::InvalidInput);
    }
    let input = if request.flags & FENCE_IN != 0 {
        Some(
            crate::drm::syncobj::import(context, request.reserved as u32 as i32)
                .map_err(|_| AxError::InvalidInput)?,
        )
    } else {
        None
    };
    let completion = Fence::new(false);
    let output = if request.flags & FENCE_OUT != 0 {
        let limit =
            context.caller_process().rlim.read()[linux_raw_sys::general::RLIMIT_NOFILE].current;
        Some(prepare_output(
            context.files().clone(),
            limit.min(crate::task::AX_FILE_LIMIT as u64) as usize,
            completion.clone(),
        )?)
    } else {
        None
    };
    exec_request(file, context, request, completion, input, |s, d, p, vm| {
        match p {
            Plan::Copy(copy) => super::gt::submit_copy(s, d, copy, vm.clone()),
            Plan::Render => super::gt::submit_render(s, d, vm),
        }
        .map_err(|_| AxError::Io)
    })?;
    finish_output(context, arg, request, output)?;
    Ok(0)
}

pub(crate) fn dispatch(
    file: &DrmFile,
    copy: &impl UserCopy,
    cmd: u32,
    arg: usize,
) -> AxResult<usize> {
    match cmd {
        CREATE => create(file, copy, arg)?,
        PREAD => data(file, copy, arg, false)?,
        PWRITE => data(file, copy, arg, true)?,
        WAIT => wait(file, copy, arg)?,
        MMAP => {
            let mut r: Mmap = read_pod(copy, arg)?;
            if r.pad != 0 || r.extensions != 0 || r.flags != 2 {
                return Err(AxError::InvalidInput);
            }
            r.offset = file.map_dumb(r.handle).map_err(AxError::from)?;
            write_pod(copy, arg, &r)?;
        }
        BUSY => {
            let mut r: Busy = read_pod(copy, arg)?;
            let o = object(file, r.handle)?;
            r.busy = if o
                .reservation
                .predecessor()
                .is_some_and(|f| !f.is_signaled())
            {
                (1 << 17) | 2
            } else {
                0
            };
            write_pod(copy, arg, &r)?;
        }
        EXEC | EXEC_WR => return Err(AxError::InvalidInput),
        _ => return super::gem_context::dispatch(file, copy, cmd, arg),
    }
    Ok(0)
}

#[cfg(test)]
pub(super) mod tests {
    use alloc::vec;
    use core::{cell::RefCell, mem::MaybeUninit};

    use super::*;
    pub(in crate::drm::intel) struct Image(pub(in crate::drm::intel) RefCell<Vec<u8>>);
    impl UserCopy for Image {
        fn read(&self, a: usize, d: &mut [MaybeUninit<u8>]) -> AxResult<()> {
            let bytes = self.0.borrow();
            let source = bytes
                .get(a..a.checked_add(d.len()).ok_or(AxError::BadAddress)?)
                .ok_or(AxError::BadAddress)?;
            for (to, from) in d.iter_mut().zip(source) {
                to.write(*from);
            }
            Ok(())
        }
        fn write(&self, a: usize, d: &[u8]) -> AxResult<()> {
            let mut bytes = self.0.borrow_mut();
            let to = bytes
                .get_mut(a..a.checked_add(d.len()).ok_or(AxError::BadAddress)?)
                .ok_or(AxError::BadAddress)?;
            to.copy_from_slice(d);
            Ok(())
        }
    }
    pub(in crate::drm::intel) struct Adapter;
    impl crate::drm::DisplayAdapter for Adapter {
        fn create_dumb(
            &self,
            _: crate::drm::DumbRequest,
            _: u32,
            _: u64,
            _: Arc<dyn Send + Sync>,
        ) -> crate::drm::DrmResult<Arc<dyn GemBacking>> {
            Err(crate::drm::DrmError::Unsupported)
        }
        fn present(&self, _: crate::drm::Scanout) -> crate::drm::DrmResult<Arc<Fence>> {
            Err(crate::drm::DrmError::Unsupported)
        }
    }
    pub(in crate::drm::intel) fn file() -> DrmFile {
        crate::drm::DrmDevice::new(Arc::new(Adapter), 1, 2, 3, 4).open_primary()
    }
    fn new(file: &DrmFile, copy: &Image) -> u32 {
        write_pod(
            copy,
            0,
            &Create {
                size: 16384,
                ..Default::default()
            },
        )
        .unwrap();
        create(file, copy, 0).unwrap();
        read_pod::<Create>(copy, 0).unwrap().handle
    }
    pub(in crate::drm::intel) fn prepare(file: &DrmFile, copy: &Image) -> (u32, u32, u32, u32) {
        let handles = [new(file, copy), new(file, copy), new(file, copy)];
        let src = object(file, handles[0])
            .unwrap()
            .backing
            .shared_pages()
            .unwrap();
        src.write_bytes(0, &vec![0x73; 16384]).unwrap();
        let batch = intel_gt::bcs::batch(intel_gt::bcs::Copy {
            source: 0x10000,
            destination: 0x20000,
            source_bytes: 16384,
            destination_bytes: 16384,
            width: 64,
            height: 64,
            pitch: 256,
        })
        .unwrap();
        let mut data = Vec::new();
        for word in &batch[3..] {
            data.extend_from_slice(&word.to_le_bytes());
        }
        object(file, handles[2])
            .unwrap()
            .backing
            .shared_pages()
            .unwrap()
            .write_bytes(0, &data)
            .unwrap();
        for (i, handle) in handles.iter().enumerate() {
            write_pod(
                copy,
                256 + i * size_of::<Object>(),
                &Object {
                    handle: *handle,
                    offset: 0x10000 * (i as u64 + 1),
                    flags: PINNED | if i == 1 { WRITE } else { 0 },
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let sync = file.create_syncobj(false).unwrap();
        write_pod(
            copy,
            512,
            &ExecFence {
                handle: sync,
                flags: 2,
            },
        )
        .unwrap();
        write_pod(
            copy,
            0,
            &Exec {
                buffers: 256,
                count: 3,
                length: 44,
                flags: 3 | NO_RELOC | FENCE_ARRAY,
                fence_count: 1,
                fences: 512,
                ..Default::default()
            },
        )
        .unwrap();
        (handles[0], handles[1], handles[2], sync)
    }
    #[test]
    fn gem_wire_copy_exec_result_reservation_syncobj_wait_and_mmap_share_owned_pages() {
        let _context = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 65536]));
        let (src, dst, batch, sync) = prepare(&file, &copy);
        exec_with(&file, &copy, 0, |s, d, p| {
            match p {
                Plan::Copy(copy) => super::super::gt::copy::tests::objects(s, d, copy),
                Plan::Render => super::super::gt::copy::tests::render_objects(s, d),
            }
            .map_err(|_| AxError::Io)
        })
        .unwrap();
        let output = file.syncobj(sync).unwrap().fence().unwrap();
        assert!(output.is_signaled());
        assert!(!output.is_failed());
        for handle in [src, dst, batch] {
            let o = object(&file, handle).unwrap();
            let f = o.reservation.predecessor().unwrap();
            assert!(Arc::ptr_eq(&f, &output));
        }
        write_pod(
            &copy,
            0,
            &Data {
                handle: dst,
                size: 16384,
                address: 4096,
                ..Default::default()
            },
        )
        .unwrap();
        data(&file, &copy, 0, false).unwrap();
        assert!(
            copy.0.borrow()[4096..4096 + 16384]
                .iter()
                .all(|&b| b == 0x73)
        );
        write_pod(
            &copy,
            0,
            &Wait {
                handle: dst,
                timeout_ns: 0,
                flags: 0,
            },
        )
        .unwrap();
        wait(&file, &copy, 0).unwrap();
        write_pod(
            &copy,
            0,
            &Mmap {
                handle: dst,
                flags: 2,
                ..Default::default()
            },
        )
        .unwrap();
        dispatch(&file, &copy, MMAP, 0).unwrap();
        let offset = read_pod::<Mmap>(&copy, 0).unwrap().offset;
        let mapping = file.mmap_object(offset).unwrap().shared_pages().unwrap();
        file.close_handle(dst).unwrap();
        let mut first = [0];
        mapping.read_bytes(0, &mut first).unwrap();
        assert_eq!(first, [0x73]);
    }
    #[test]
    fn fixed_rcs_user_page_reaches_same_gem_sync_and_private_vm_renderer_model() {
        let _context = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 65536]));
        let mut handles = [0; 3];
        for h in &mut handles {
            write_pod(
                &copy,
                0,
                &Create {
                    size: 24576,
                    ..Default::default()
                },
            )
            .unwrap();
            create(&file, &copy, 0).unwrap();
            *h = read_pod::<Create>(&copy, 0).unwrap().handle;
        }
        let mut bytes = vec![0xa5; 24576];
        for (i, b) in bytes[4096..4096 + 16384].iter_mut().enumerate() {
            *b = if i % 4 == 3 {
                255
            } else {
                (i as u8).wrapping_mul(29)
            };
        }
        object(&file, handles[0])
            .unwrap()
            .backing
            .shared_pages()
            .unwrap()
            .write_bytes(0, &bytes)
            .unwrap();
        let batch = object(&file, handles[2])
            .unwrap()
            .backing
            .shared_pages()
            .unwrap();
        for (i, w) in intel_gt::rcs_page::PAGE.iter().enumerate() {
            batch.write_bytes(i * 4, &w.to_le_bytes()).unwrap();
        }
        for (i, h) in handles.iter().enumerate() {
            write_pod(
                &copy,
                256 + i * size_of::<Object>(),
                &Object {
                    handle: *h,
                    offset: 0x10000 * (i as u64 + 1),
                    flags: PINNED | if i == 1 { WRITE } else { 0 },
                    ..Default::default()
                },
            )
            .unwrap();
        }
        write_pod(
            &copy,
            0,
            &Exec {
                buffers: 256,
                count: 3,
                length: intel_gt::rcs_page::COMMAND_BYTES,
                flags: 1 | NO_RELOC,
                ..Default::default()
            },
        )
        .unwrap();
        exec_with(&file, &copy, 0, |s, d, p| match p {
            Plan::Render => {
                super::super::gt::copy::tests::render_objects(s, d).map_err(|_| AxError::Io)
            }
            _ => panic!("not BCS"),
        })
        .unwrap();
        let destination = object(&file, handles[1]).unwrap();
        let mut result = vec![0; 16384];
        destination
            .backing
            .shared_pages()
            .unwrap()
            .read_bytes(4096, &mut result)
            .unwrap();
        assert_eq!(result, &bytes[4096..4096 + 16384]);
        assert!(destination.reservation.predecessor().unwrap().is_signaled());
        // Arbitrary shader edits must never be submitted privileged.
        batch.write_bytes(0, &0u32.to_le_bytes()).unwrap();
        assert_eq!(
            exec_with(&file, &copy, 0, |_, _, _| panic!("must not submit")),
            Err(AxError::InvalidInput)
        );
    }
    #[test]
    fn user_signal_during_exec_does_not_complete_gem_reservations_or_captured_output_fence() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 65536]));
        let (source, destination, _, out) = prepare(&file, &copy);
        let output = file.syncobj(out).unwrap();
        exec_with(&file, &copy, 0, |src, dst, plan| {
            let producer = output.fence().unwrap();
            assert!(!producer.is_signaled());
            output.signal();
            assert!(output.fence().unwrap().is_signaled());
            assert!(!producer.is_signaled());
            for handle in [source, destination] {
                assert!(
                    !object(&file, handle)
                        .unwrap()
                        .reservation
                        .predecessor()
                        .unwrap()
                        .is_signaled()
                );
            }
            let Plan::Copy(operation) = plan else {
                panic!("copy")
            };
            super::super::gt::copy::tests::objects(src, dst, operation).map_err(|_| AxError::Io)
        })
        .unwrap();
        assert!(
            object(&file, destination)
                .unwrap()
                .reservation
                .predecessor()
                .unwrap()
                .is_signaled()
        );
        let mut bytes = [0; 16384];
        object(&file, destination)
            .unwrap()
            .backing
            .shared_pages()
            .unwrap()
            .read_bytes(0, &mut bytes)
            .unwrap();
        assert!(bytes.iter().all(|v| *v == 0x73));
    }
    #[test]
    fn timeline_extension_uses_existing_gem_completion_and_preserves_producer_ownership() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 65536]));
        let (_, destination, _, out) = prepare(&file, &copy);
        let mut request: Exec = read_pod(&copy, 0).unwrap();
        request.flags = (request.flags & !FENCE_ARRAY) | EXTENSIONS;
        request.fence_count = 0;
        request.fences = 768;
        write_pod(&copy, 0, &request).unwrap();
        write_pod(
            &copy,
            768,
            &TimelineExt {
                count: 1,
                handles: 512,
                values: 1024,
                ..Default::default()
            },
        )
        .unwrap();
        write_pod(&copy, 1024, &1u64).unwrap();
        let object = file.syncobj(out).unwrap();
        exec_with(&file, &copy, 0, |source, destination, plan| {
            let producer = object.fence_at(1).unwrap();
            assert!(!producer.is_signaled());
            object.signal_point(2).unwrap();
            assert!(!producer.is_signaled());
            assert!(!object.fence_at(2).unwrap().is_signaled());
            let Plan::Copy(operation) = plan else {
                panic!("copy")
            };
            super::super::gt::copy::tests::objects(source, destination, operation)
                .map_err(|_| AxError::Io)
        })
        .unwrap();
        assert!(object.fence_at(2).unwrap().is_signaled());
        assert_eq!(object.query_point(false), 2);
        let mut bytes = [0; 16384];
        super::object(&file, destination)
            .unwrap()
            .backing
            .shared_pages()
            .unwrap()
            .read_bytes(0, &mut bytes)
            .unwrap();
        assert!(bytes.iter().all(|v| *v == 0x73));
    }
    #[test]
    fn failed_timeline_submission_errors_output_and_gem_with_the_same_device_leaf() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 65536]));
        let (source, destination, _, out) = prepare(&file, &copy);
        let mut request: Exec = read_pod(&copy, 0).unwrap();
        request.flags = (request.flags & !FENCE_ARRAY) | EXTENSIONS;
        request.fence_count = 0;
        request.fences = 768;
        write_pod(&copy, 0, &request).unwrap();
        write_pod(
            &copy,
            768,
            &TimelineExt {
                count: 1,
                handles: 512,
                values: 1024,
                ..Default::default()
            },
        )
        .unwrap();
        write_pod(&copy, 1024, &1u64).unwrap();
        assert_eq!(
            exec_with(&file, &copy, 0, |_, _, _| Err(AxError::Io)),
            Err(AxError::Io)
        );
        let output = file.syncobj(out).unwrap().fence_at(1).unwrap();
        assert!(output.is_failed());
        for handle in [source, destination] {
            assert!(Arc::ptr_eq(
                &output,
                &object(&file, handle)
                    .unwrap()
                    .reservation
                    .predecessor()
                    .unwrap()
            ));
        }
    }
    #[test]
    fn malformed_timeline_extension_is_refused_before_gem_fences_are_published() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 65536]));
        let (source, ..) = prepare(&file, &copy);
        let mut request: Exec = read_pod(&copy, 0).unwrap();
        request.flags = (request.flags & !FENCE_ARRAY) | EXTENSIONS;
        request.fence_count = 0;
        request.fences = 768;
        write_pod(&copy, 0, &request).unwrap();
        write_pod(&copy, 1024, &1u64).unwrap();
        for ext in [
            TimelineExt {
                count: 65,
                ..Default::default()
            },
            TimelineExt {
                next: 768,
                ..Default::default()
            },
            TimelineExt {
                name: 1,
                ..Default::default()
            },
            TimelineExt {
                flags: 1,
                ..Default::default()
            },
            TimelineExt {
                reserved: [1, 0, 0, 0],
                ..Default::default()
            },
        ] {
            write_pod(&copy, 768, &ext).unwrap();
            assert_eq!(
                exec_with(&file, &copy, 0, |_, _, _| panic!("invalid extension")),
                Err(AxError::InvalidInput)
            );
            assert!(
                object(&file, source)
                    .unwrap()
                    .reservation
                    .predecessor()
                    .is_none()
            );
        }
        write_pod(
            &copy,
            768,
            &TimelineExt {
                count: 1,
                handles: 512,
                values: 1024,
                ..Default::default()
            },
        )
        .unwrap();
        let mut fence: ExecFence = read_pod(&copy, 512).unwrap();
        fence.flags = 3;
        write_pod(&copy, 512, &fence).unwrap();
        assert_eq!(
            exec_with(&file, &copy, 0, |_, _, _| panic!(
                "same nonzero WAIT/SIGNAL"
            )),
            Err(AxError::InvalidInput)
        );
    }
    #[test]
    fn invalid_batch_or_alias_is_refused_before_any_completion_publication() {
        let _context = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 65536]));
        let (src, _dst, batch, sync) = prepare(&file, &copy);
        let pages = object(&file, batch)
            .unwrap()
            .backing
            .shared_pages()
            .unwrap();
        pages.write_bytes(0, &0x11000001u32.to_le_bytes()).unwrap(); // arbitrary LRI
        assert_eq!(
            exec_with(&file, &copy, 0, |_, _, _| panic!("must not execute")),
            Err(AxError::InvalidInput)
        );
        assert!(file.syncobj(sync).unwrap().fence().is_err());
        assert!(
            object(&file, src)
                .unwrap()
                .reservation
                .predecessor()
                .is_none()
        );
        let (_src, _dst, _batch, _sync) = prepare(&file, &copy);
        let mut dst: Object = read_pod(&copy, 256 + size_of::<Object>()).unwrap();
        dst.handle = read_pod::<Object>(&copy, 256).unwrap().handle;
        write_pod(&copy, 256 + size_of::<Object>(), &dst).unwrap();
        assert_eq!(
            exec_with(&file, &copy, 0, |_, _, _| panic!("must not execute")),
            Err(AxError::InvalidInput)
        );
    }
    #[test]
    fn native_submission_error_signals_same_error_fence_to_all_objects_and_output() {
        let _context = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 65536]));
        let (src, dst, batch, sync) = prepare(&file, &copy);
        assert_eq!(
            exec_with(&file, &copy, 0, |_, _, _| Err(AxError::Io)),
            Err(AxError::Io)
        );
        let f = file.syncobj(sync).unwrap().fence().unwrap();
        assert!(f.is_failed());
        for h in [src, dst, batch] {
            assert!(Arc::ptr_eq(
                &f,
                &object(&file, h).unwrap().reservation.predecessor().unwrap()
            ));
        }
        write_pod(
            &copy,
            0,
            &Wait {
                handle: dst,
                flags: 0,
                timeout_ns: 0,
            },
        )
        .unwrap();
        assert_eq!(wait(&file, &copy, 0), Err(AxError::Io));
    }
    #[test]
    fn pwrite_fault_does_not_mutate_gem_and_busy_wait_poll_tracks_real_fences() {
        let _context = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 1024]));
        let h = new(&file, &copy);
        write_pod(
            &copy,
            0,
            &Data {
                handle: h,
                offset: 4,
                size: 8,
                address: 1020,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(data(&file, &copy, 0, true), Err(AxError::BadAddress));
        let o = object(&file, h).unwrap();
        assert!(o.reservation.predecessor().is_none());
        let pending = Fence::new(false);
        o.reservation.publish(pending.clone());
        write_pod(
            &copy,
            0,
            &Wait {
                handle: h,
                flags: 0,
                timeout_ns: 0,
            },
        )
        .unwrap();
        assert_eq!(wait(&file, &copy, 0), Err(AxError::TimedOut));
        write_pod(&copy, 0, &Busy { handle: h, busy: 0 }).unwrap();
        dispatch(&file, &copy, BUSY, 0).unwrap();
        assert_ne!(read_pod::<Busy>(&copy, 0).unwrap().busy, 0);
        pending.signal();
    }
    #[test]
    fn sync_file_exec_publishes_same_completion_cloexec_only_after_copyout() {
        let _context = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 65536]));
        let (_, dst, _, sync) = prepare(&file, &copy);
        let mut request: Exec = read_pod(&copy, 0).unwrap();
        request.flags |= FENCE_IN | FENCE_OUT;
        request.reserved = 17 | (0xdead_beef_u64 << 32);
        let completion = Fence::new(false);
        let table = Arc::new(crate::file::FdTable::new().unwrap());
        let output = prepare_output(table.clone(), 8, completion.clone()).unwrap();
        let fd = output.fd();
        assert!(table.get_description(fd).is_err());
        let input = Fence::new(true);
        exec_request(
            &file,
            &copy,
            request,
            completion.clone(),
            Some(input),
            |_, _, _, _| {
                assert!(!completion.is_signaled());
                assert!(table.get_description(fd).is_err());
                assert!(Arc::ptr_eq(
                    &object(&file, dst)
                        .unwrap()
                        .reservation
                        .predecessor()
                        .unwrap(),
                    &completion
                ));
                Ok(())
            },
        )
        .unwrap();
        finish_output(&copy, 0, request, Some(output)).unwrap();
        let result: Exec = read_pod(&copy, 0).unwrap();
        assert_eq!(result.reserved as u32, 17);
        assert_eq!((result.reserved >> 32) as i32, fd);
        assert!(table.get_cloexec(fd).unwrap());
        let exported = table
            .get_description(fd)
            .unwrap()
            .inner
            .clone()
            .downcast_arc::<crate::drm::syncobj::SyncFile>()
            .ok()
            .unwrap();
        assert!(Arc::ptr_eq(&exported.fence(), &completion));
        assert!(Arc::ptr_eq(
            &file.syncobj(sync).unwrap().fence().unwrap(),
            &completion
        ));
        assert!(completion.is_signaled() && !completion.is_failed());
    }
    #[test]
    fn sync_file_output_failures_release_unpublished_slot_not_gpu_ownership() {
        let _context = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 65536]));
        let (_, dst, ..) = prepare(&file, &copy);
        let mut request: Exec = read_pod(&copy, 0).unwrap();
        request.flags |= FENCE_OUT;
        let completion = Fence::new(false);
        let table = Arc::new(crate::file::FdTable::new().unwrap());
        let output = prepare_output(table.clone(), 1, completion.clone()).unwrap();
        assert!(matches!(
            prepare_output(table.clone(), 1, completion.clone()),
            Err(AxError::TooManyOpenFiles)
        ));
        exec_request(
            &file,
            &copy,
            request,
            completion.clone(),
            None,
            |_, _, _, _| Ok(()),
        )
        .unwrap();
        assert_eq!(
            finish_output(&copy, 65535, request, Some(output)),
            Err(AxError::BadAddress)
        );
        assert!(table.get_description(0).is_err());
        assert!(completion.is_signaled() && !completion.is_failed());
        assert!(Arc::ptr_eq(
            &object(&file, dst)
                .unwrap()
                .reservation
                .predecessor()
                .unwrap(),
            &completion
        ));
        let output = prepare_output(table.clone(), 1, Fence::new(false)).unwrap();
        assert_eq!(output.fd(), 0);
        drop(output);
        let error = Fence::new(false);
        let output = prepare_output(table.clone(), 1, error.clone()).unwrap();
        assert_eq!(
            exec_request(
                &file,
                &copy,
                request,
                error.clone(),
                None,
                |_, _, _, _| Err(AxError::Io)
            ),
            Err(AxError::Io)
        );
        drop(output);
        assert!(table.get_description(0).is_err());
        assert!(error.is_failed());
    }
    #[test]
    fn failed_sync_file_input_prevents_execution_and_reservation_publication() {
        let _context = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 65536]));
        let (_, dst, ..) = prepare(&file, &copy);
        let mut request: Exec = read_pod(&copy, 0).unwrap();
        request.flags |= FENCE_IN;
        let input = Fence::new(false);
        input.signal_error();
        assert_eq!(
            exec_request(
                &file,
                &copy,
                request,
                Fence::new(false),
                Some(input),
                |_, _, _, _| panic!("failed producer executed")
            ),
            Err(AxError::Io)
        );
        assert!(
            object(&file, dst)
                .unwrap()
                .reservation
                .predecessor()
                .is_none()
        );
        assert_eq!(
            exec_request(
                &file,
                &copy,
                request,
                Fence::new(false),
                None,
                |_, _, _, _| panic!("missing producer executed")
            ),
            Err(AxError::InvalidInput)
        );
    }
}
