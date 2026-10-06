// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. Repository MIT license.
// VM/proto-context lifetime adaptation follows Linux7.2.3
// gem/i915_gem_context.c i915_gem_vm_{create,destroy}_ioctl/get_ppgtt/
// set_proto_ctx_vm/create_setparam. Copyright © 2011-2012 Intel Corporation.
// Full MIT grant: crates/ax/tk-intel-gt/LICENSE-MIT.
//! Original bounded i915 context/query transport over existing per-file GEM.
//! Wire/errno facts from Linux7.2.3 i915_drm.h/i915_query.c/i915_getparam.c;
//! no upstream implementation body is copied. Hardware facts use ported GT IO.
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::mem::size_of;

use axerrno::{AxError, AxResult};

use super::gem_exec::command;
use crate::drm::{
    DrmFile,
    ioctl::{UserCopy, read_pod, write_pod},
};
// Original per-file context lifetime adapter. Every admitted job rebuilds a
// complete hardware image; PPGTT ownership can now persist/share across
// contexts. Graphics register-state save and arbitrary residency are separate.
pub(crate) struct JobContext {
    vm: Option<Arc<super::gt::copy::Vm>>,
    started: bool,
    engines: Option<Vec<u16>>,
    images: BTreeMap<u16, Arc<super::gt::copy::SavedContext>>,
}
impl JobContext {
    fn new() -> Self {
        Self {
            vm: None,
            started: false,
            engines: None,
            images: BTreeMap::new(),
        }
    }
    pub(super) fn render_engine(&self, selector: u16) -> AxResult<bool> {
        let class = if let Some(engines) = &self.engines {
            *engines
                .get(usize::from(selector))
                .ok_or(AxError::InvalidInput)?
        } else {
            match selector {
                0 | 1 => 0,
                3 => 1,
                _ => return Err(AxError::InvalidInput),
            }
        };
        match class {
            0 => Ok(true),
            1 => Ok(false),
            _ => Err(AxError::InvalidInput),
        }
    }
    pub(super) fn image(
        &mut self,
        file: &DrmFile,
        selector: u16,
        render: bool,
    ) -> AxResult<Arc<super::gt::copy::SavedContext>> {
        let slot = if self.engines.is_none() {
            if render { 0 } else { 3 }
        } else {
            selector
        };
        if let Some(image) = self.images.get(&slot) {
            return Ok(image.clone());
        }
        let image = super::gt::copy::SavedContext::new(file, render)?;
        self.images.insert(slot, image.clone());
        Ok(image)
    }
    pub(super) fn vm(&mut self, file: &DrmFile) -> AxResult<Arc<super::gt::copy::Vm>> {
        if self.vm.is_none() {
            self.vm = Some(super::gt::copy::Vm::new_for_file(file)?);
        }
        Ok(self.vm.as_ref().unwrap().clone())
    }
    pub(super) fn begin(&mut self, file: &DrmFile) -> AxResult<Arc<super::gt::copy::Vm>> {
        let vm = self.vm(file)?;
        self.started = true;
        Ok(vm)
    }
}
pub(crate) struct Contexts {
    default: Arc<axsync::Mutex<JobContext>>,
    state: spin::Mutex<ContextState>,
}
struct ContextState {
    next: u32,
    jobs: BTreeMap<u32, Arc<axsync::Mutex<JobContext>>>,
    next_vm: u32,
    vms: BTreeMap<u32, Arc<super::gt::copy::Vm>>,
}
impl Contexts {
    pub(crate) fn new() -> Self {
        Self {
            default: Arc::new(axsync::Mutex::new(JobContext::new())),
            state: spin::Mutex::new(ContextState {
                next: 1,
                jobs: BTreeMap::new(),
                next_vm: 1,
                vms: BTreeMap::new(),
            }),
        }
    }
    fn create(&self) -> AxResult<u32> {
        self.create_vm(None)
    }
    fn create_vm(&self, vm: Option<Arc<super::gt::copy::Vm>>) -> AxResult<u32> {
        let mut job = JobContext::new();
        job.vm = vm;
        self.create_job(job)
    }
    fn create_job(&self, job: JobContext) -> AxResult<u32> {
        let gate = Arc::try_new(axsync::Mutex::new(job)).map_err(|_| AxError::NoMemory)?;
        let mut state = self.state.lock();
        if state.jobs.len() >= 256 {
            return Err(AxError::NoMemory);
        }
        let id = state.next;
        state.next = id.checked_add(1).ok_or(AxError::NoMemory)?;
        state.jobs.insert(id, gate);
        Ok(id)
    }
    pub(super) fn lookup(&self, id: u32) -> AxResult<Arc<axsync::Mutex<JobContext>>> {
        if id == 0 {
            Ok(self.default.clone())
        } else {
            self.state
                .lock()
                .jobs
                .get(&id)
                .cloned()
                .ok_or(AxError::NotFound)
        }
    }
    fn publish_vm(&self, vm: Arc<super::gt::copy::Vm>) -> AxResult<u32> {
        let mut state = self.state.lock();
        if state.vms.len() >= 256 {
            return Err(AxError::NoMemory);
        }
        let id = state.next_vm;
        state.next_vm = id.checked_add(1).ok_or(AxError::NoMemory)?;
        state.vms.insert(id, vm);
        Ok(id)
    }
    fn vm(&self, id: u32) -> AxResult<Arc<super::gt::copy::Vm>> {
        self.state
            .lock()
            .vms
            .get(&id)
            .cloned()
            .ok_or(AxError::NotFound)
    }
    fn destroy_vm(&self, id: u32) -> AxResult<()> {
        self.state
            .lock()
            .vms
            .remove(&id)
            .map(|_| ())
            .ok_or(AxError::NotFound)
    }
    fn set_vm(&self, context: u32, id: u32) -> AxResult<()> {
        let vm = self.vm(id)?;
        let context = self.lookup(context)?;
        let mut context = context.lock();
        // Linux only changes VM while a proto-context is mutable.
        if context.started {
            return Err(AxError::InvalidInput);
        }
        context.vm = Some(vm);
        Ok(())
    }
    fn destroy(&self, id: u32) -> AxResult<()> {
        if id == 0 {
            return Err(AxError::NotFound);
        }
        self.state
            .lock()
            .jobs
            .remove(&id)
            .map(|_| ())
            .ok_or(AxError::NotFound)
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct RegisterRead {
    offset: u64,
    value: u64,
}
const REG_READ: u32 = command::<RegisterRead>(0x31, 3);
fn register_read(
    copy: &impl UserCopy,
    arg: usize,
    timestamp: impl FnOnce() -> AxResult<u64>,
) -> AxResult<()> {
    let mut r: RegisterRead = read_pod(copy, arg)?;
    // Target Mesa uses this source whitelist entry plus I915_REG_READ_8B_WA.
    // Do not expose arbitrary GT MMIO or pretend two dword reads are readq.
    if r.offset != 0x2359 {
        return Err(AxError::InvalidInput);
    }
    r.value = timestamp()?;
    write_pod(copy, arg, &r)
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct SetparamExt {
    next: u64,
    name: u32,
    flags: u32,
    reserved: [u32; 4],
    param: ContextParam,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct VmControl {
    extensions: u64,
    flags: u32,
    id: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct ContextId {
    id: u32,
    flags: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct ContextExt {
    id: u32,
    flags: u32,
    extensions: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct ContextParam {
    id: u32,
    size: u32,
    param: u64,
    value: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Getparam {
    param: i32,
    padding: u32,
    value: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Query {
    count: u32,
    flags: u32,
    items: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct QueryItem {
    id: u64,
    length: i32,
    flags: u32,
    data: u64,
}
const CTX_CREATE: u32 = command::<ContextId>(0x2d, 3);
const CTX_EXT: u32 = command::<ContextExt>(0x2d, 3);
const CTX_DESTROY: u32 = command::<ContextId>(0x2e, 1);
const CTX_GETPARAM: u32 = command::<ContextParam>(0x34, 3);
const CTX_SETPARAM: u32 = command::<ContextParam>(0x35, 3);
const VM_CREATE: u32 = command::<VmControl>(0x3a, 3);
const VM_DESTROY: u32 = command::<VmControl>(0x3b, 1);
const GETPARAM: u32 = command::<Getparam>(6, 3);
const QUERY: u32 = command::<Query>(0x39, 3);
// Independently compiled Linux7.2.3 x86_64 header facts.
const _: () = {
    assert!(size_of::<ContextId>() == 8 && size_of::<ContextExt>() == 16);
    assert!(size_of::<VmControl>() == 16 && size_of::<SetparamExt>() == 56);
    assert!(VM_CREATE == 0xc010647a && VM_DESTROY == 0x4010647b && CTX_SETPARAM == 0xc0186475);
    assert!(size_of::<ContextParam>() == 24 && size_of::<Getparam>() == 16);
    assert!(size_of::<Query>() == 16 && size_of::<QueryItem>() == 24);
    assert!(CTX_CREATE == 0xc008646d && CTX_EXT == 0xc010646d && CTX_DESTROY == 0x4008646e);
    assert!(CTX_GETPARAM == 0xc0186474 && GETPARAM == 0xc0106446 && QUERY == 0xc0106479);
    assert!(
        core::mem::offset_of!(Getparam, value) == 8
            && core::mem::offset_of!(QueryItem, length) == 8
            && core::mem::offset_of!(QueryItem, data) == 16
    );
};

#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Engine {
    class: u16,
    instance: u16,
}
fn engine_map(
    copy: &impl UserCopy,
    param: &ContextParam,
    render_available: bool,
) -> AxResult<Vec<u16>> {
    if param.size < 8 || !(param.size - 8).is_multiple_of(4) || (param.size - 8) / 4 > 64 {
        return Err(AxError::InvalidInput);
    }
    let address = usize::try_from(param.value).map_err(|_| AxError::BadAddress)?;
    let mut engines = Vec::new();
    engines
        .try_reserve_exact(((param.size - 8) / 4) as usize)
        .map_err(|_| AxError::NoMemory)?;
    // Source checks class-instance records before the extension header.
    for i in 0..(param.size - 8) / 4 {
        let record: Engine = read_pod(
            copy,
            address
                .checked_add(8 + i as usize * 4)
                .ok_or(AxError::BadAddress)?,
        )?;
        if record.class == u16::MAX && record.instance == u16::MAX {
            engines.push(u16::MAX);
            continue;
        }
        if record.instance != 0
            || !matches!(record.class, 0 | 1)
            || (record.class == 0 && !render_available)
        {
            return Err(AxError::NotFound);
        }
        engines.push(record.class);
    }
    let extensions: u64 = read_pod(copy, address)?;
    if extensions != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(engines)
}
fn proto_param(
    file: &DrmFile,
    copy: &impl UserCopy,
    job: &mut JobContext,
    param: &ContextParam,
    render_available: bool,
) -> AxResult<()> {
    match param.param {
        9 => {
            if param.size != 0 {
                return Err(AxError::InvalidInput);
            }
            if param.value > u32::MAX as u64 {
                return Err(AxError::NotFound);
            }
            job.vm = Some(file.intel_contexts.vm(param.value as u32)?);
        }
        10 => {
            if job.engines.is_some() {
                return Err(AxError::InvalidInput);
            }
            job.engines = Some(engine_map(copy, param, render_available)?);
        }
        // Native failure currently wedges the owned engine graph. Do not claim
        // reset recovery/replay or priority scheduling that does not exist.
        8 | 6 => {
            if param.size != 0 || param.value != 0 {
                return Err(AxError::InvalidInput);
            }
        }
        _ => return Err(AxError::InvalidInput),
    }
    Ok(())
}
fn context_create(
    file: &DrmFile,
    copy: &impl UserCopy,
    arg: usize,
    extended: bool,
) -> AxResult<()> {
    context_create_with(file, copy, arg, extended, super::gt::render_registered())
}
fn context_create_with(
    file: &DrmFile,
    copy: &impl UserCopy,
    arg: usize,
    extended: bool,
    render_available: bool,
) -> AxResult<()> {
    let mut request: ContextExt = if extended {
        read_pod(copy, arg)?
    } else {
        let id: ContextId = read_pod(copy, arg)?;
        ContextExt {
            id: id.id,
            flags: id.flags,
            extensions: 0,
        }
    };
    if request.flags & !3 != 0 || (!extended && request.flags != 0) {
        return Err(AxError::InvalidInput);
    }
    let mut job = JobContext::new();
    if request.flags & 1 != 0 {
        let mut address = request.extensions;
        let mut count = 0;
        while address != 0 {
            count += 1;
            if count > 8 {
                return Err(AxError::InvalidInput);
            }
            let ext: SetparamExt = read_pod(
                copy,
                usize::try_from(address).map_err(|_| AxError::BadAddress)?,
            )?;
            if ext.name != 0 || ext.flags != 0 || ext.reserved != [0; 4] || ext.param.id != 0 {
                return Err(AxError::InvalidInput);
            }
            proto_param(file, copy, &mut job, &ext.param, render_available)?;
            address = ext.next;
        }
    }
    // Linux ignores the pointer without USE_EXTENSIONS; never dereference it.
    request.id = file.intel_contexts.create_job(job)?;
    let result = if extended {
        write_pod(copy, arg, &request)
    } else {
        write_pod(
            copy,
            arg,
            &ContextId {
                id: request.id,
                flags: 0,
            },
        )
    };
    if result.is_err() {
        let _ = file.intel_contexts.destroy(request.id);
    }
    result
}
fn getparam_value(
    param: i32,
    topology: impl FnOnce() -> AxResult<intel_gt::info::Topology>,
    clock: impl FnOnce() -> AxResult<u32>,
) -> AxResult<i32> {
    Ok(match param {
        // Exact device/revision are already established by GT boot admission.
        4 => 0x46d0,
        32 => 0,
        5 | 9 | 11 | 19 | 24 | 25 | 26 | 37 | 44 | 48 | 49 | 55 => 1,
        40 => 4, // WC/WB/UC offsets; WC/UC require confirmed CPU palette, no legacy GTT mmap.
        33 => topology()?.dss.count_ones() as i32,
        34 => topology()?.eu_total() as i32,
        46 => {
            topology()?;
            1
        }
        47 => i32::from(topology()?.dss),
        50 => super::gt::context_isolation_classes() as i32, /* only completed native default-state captures. */
        51 => i32::try_from(clock()?).map_err(|_| AxError::InvalidInput)?,
        6..=8
        | 10
        | 12..=18
        | 20..=23
        | 27..=31
        | 35
        | 36
        | 38
        | 39
        | 41..=45
        | 52..=54
        | 56..=59 => 0,
        1..=3 => return Err(AxError::NoSuchDevice),
        _ => return Err(AxError::InvalidInput),
    })
}
fn engines_wire(render: bool) -> Vec<u8> {
    let count = if render { 2 } else { 1 };
    let mut bytes = alloc::vec![0u8;16+count*56];
    bytes[..4].copy_from_slice(&(count as u32).to_le_bytes());
    for i in 0..count {
        let class: u16 = if render && i == 0 { 0 } else { 1 };
        bytes[16 + i * 56..18 + i * 56].copy_from_slice(&class.to_le_bytes());
        bytes[24 + i * 56..32 + i * 56].copy_from_slice(&1u64.to_le_bytes()); // logical instance0 valid
    }
    bytes
}
fn query_with(
    copy: &impl UserCopy,
    arg: usize,
    render: bool,
    topology: impl Fn() -> AxResult<intel_gt::info::Topology>,
) -> AxResult<()> {
    let query: Query = read_pod(copy, arg)?;
    if query.flags != 0 || query.count > 64 {
        return Err(AxError::InvalidInput);
    }
    // A zero-item query does not dereference its pointer.
    for index in 0..query.count as usize {
        // Preserve source ordered usercopy: a later bad item must not erase
        // earlier query outputs, and overlapping data can change later items.
        let item_address = query
            .items
            .checked_add((index * size_of::<QueryItem>()) as u64)
            .and_then(|a| usize::try_from(a).ok())
            .ok_or(AxError::BadAddress)?;
        let item: QueryItem = read_pod(copy, item_address)?;
        if item.id == 0 {
            return Err(AxError::InvalidInput);
        }
        let result = (|| -> AxResult<i32> {
            if item.flags != 0 {
                return Err(AxError::InvalidInput);
            }
            let bytes = match item.id {
                1 => topology()?.wire().to_vec(),
                2 => engines_wire(render),
                _ => return Err(AxError::InvalidInput),
            };
            if item.length == 0 {
                return Ok(bytes.len() as i32);
            }
            if item.length < bytes.len() as i32 {
                return Err(AxError::InvalidInput);
            }
            // Source reads the request header before writing its output, so a
            // write-only/invalid pointer is not accepted by the size probe.
            let address = usize::try_from(item.data).map_err(|_| AxError::BadAddress)?;
            if item.id == 1 {
                let _: [u8; 16] = read_pod(copy, address)?;
            } else {
                let header: [u32; 4] = read_pod(copy, address)?;
                if header.iter().any(|v| *v != 0) {
                    return Err(AxError::InvalidInput);
                }
            }
            copy.write(address, &bytes)?;
            Ok(bytes.len() as i32)
        })();
        let length = match result {
            Ok(n) => n,
            Err(e) => -axerrno::LinuxError::from(e).code(),
        };
        if item.length != length {
            let address = item_address.checked_add(8).ok_or(AxError::BadAddress)?;
            write_pod(copy, address, &length)?;
        }
    }
    Ok(())
}
pub(super) fn dispatch(
    file: &DrmFile,
    copy: &impl UserCopy,
    cmd: u32,
    arg: usize,
) -> AxResult<usize> {
    match cmd {
        CTX_CREATE | CTX_EXT => context_create(file, copy, arg, cmd == CTX_EXT)?,
        CTX_DESTROY => {
            let r: ContextId = read_pod(copy, arg)?;
            if r.flags != 0 {
                return Err(AxError::InvalidInput);
            }
            file.intel_contexts.destroy(r.id)?;
        }
        CTX_GETPARAM => {
            let mut r: ContextParam = read_pod(copy, arg)?;
            let context = file.intel_contexts.lookup(r.id)?;
            if r.size != 0 || !matches!(r.param, 3 | 9 | 6 | 8) {
                return Err(AxError::InvalidInput);
            }
            r.value = if r.param == 3 {
                context.lock().begin(file)?;
                1u64 << 48
            } else if r.param == 9 {
                let vm = context.lock().begin(file)?;
                u64::from(file.intel_contexts.publish_vm(vm)?)
            } else {
                0
            };
            if let Err(error) = write_pod(copy, arg, &r) {
                if r.param == 9 {
                    let _ = file.intel_contexts.destroy_vm(r.value as u32);
                }
                return Err(error);
            }
        }
        CTX_SETPARAM => {
            let r: ContextParam = read_pod(copy, arg)?;
            let context = file.intel_contexts.lookup(r.id)?;
            let mut job = context.lock();
            if matches!(r.param, 9 | 10) && job.started {
                return Err(AxError::InvalidInput);
            }
            proto_param(file, copy, &mut job, &r, super::gt::render_registered())?;
        }
        VM_CREATE | VM_DESTROY => {
            let mut r: VmControl = read_pod(copy, arg)?;
            if r.extensions != 0 || r.flags != 0 {
                return Err(AxError::InvalidInput);
            }
            if cmd == VM_CREATE {
                let vm = super::gt::copy::Vm::new_for_file(file)?;
                r.id = file.intel_contexts.publish_vm(vm)?;
                if let Err(error) = write_pod(copy, arg, &r) {
                    let _ = file.intel_contexts.destroy_vm(r.id);
                    return Err(error);
                }
            } else {
                file.intel_contexts.destroy_vm(r.id)?;
            }
        }
        GETPARAM => {
            let r: Getparam = read_pod(copy, arg)?;
            let value = getparam_value(
                r.param,
                || super::gt::topology().map_err(|_| AxError::NoSuchDevice),
                || super::gt::clock_frequency().map_err(|_| AxError::NoSuchDevice),
            )?;
            write_pod(
                copy,
                usize::try_from(r.value).map_err(|_| AxError::BadAddress)?,
                &value,
            )?;
        }
        REG_READ => register_read(copy, arg, || {
            super::gt::timestamp().map_err(|_| AxError::Io)
        })?,
        QUERY => query_with(copy, arg, super::gt::render_registered(), || {
            super::gt::topology().map_err(|_| AxError::NoSuchDevice)
        })?,
        _ => return Err(AxError::OperationNotSupported),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::{cell::RefCell, mem::MaybeUninit};

    use super::*;
    use crate::drm::intel::gem_exec::{
        Exec, Plan, exec_with, object,
        tests::{Adapter, Image, file, prepare},
    };
    #[test]
    fn implemented_iris_softpin_batch_selectors_report_their_source_uapi_capabilities() {
        for param in [26, 48] {
            assert_eq!(
                getparam_value(
                    param,
                    || panic!("not a fuse query"),
                    || panic!("not a clock query")
                )
                .unwrap(),
                1
            );
        }
        let mut job = JobContext::new();
        assert!(!job.render_engine(3).unwrap()); // legacy I915_EXEC_BLT
        job.engines = Some(alloc::vec![0, 0, 1]); // source RCS/RCS/COPY classes
        assert!(job.render_engine(0).unwrap());
        assert!(job.render_engine(1).unwrap());
        assert!(!job.render_engine(2).unwrap());
    }
    #[test]
    fn created_context_is_file_local_and_retained_across_destroy_for_admitted_jobs() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let file = file();
        let other = crate::drm::DrmDevice::new(Arc::new(Adapter), 1, 2, 3, 4).open_primary();
        let copy = Image(RefCell::new(vec![0; 65536]));
        write_pod(&copy, 0, &ContextId::default()).unwrap();
        dispatch(&file, &copy, CTX_CREATE, 0).unwrap();
        let id = read_pod::<ContextId>(&copy, 0).unwrap().id;
        assert!(id != 0 && other.intel_contexts.lookup(id).is_err());
        let context = file.intel_contexts.lookup(id).unwrap();
        let weak = Arc::downgrade(&context);
        file.intel_contexts.destroy(id).unwrap();
        assert!(file.intel_contexts.lookup(id).is_err());
        assert!(weak.upgrade().is_some());
        drop(context);
        assert!(weak.upgrade().is_none());
        assert_eq!(file.intel_contexts.destroy(0), Err(AxError::NotFound));
        // A later create never reuses the destroyed identity.
        let fresh = file.intel_contexts.create().unwrap();
        assert!(fresh > id);
        let (src, dst, ..) = prepare(&file, &copy);
        let mut request: Exec = read_pod(&copy, 0).unwrap();
        request.context = u64::from(fresh);
        write_pod(&copy, 0, &request).unwrap();
        exec_with(&file, &copy, 0, |s, d, plan| {
            let Plan::Copy(operation) = plan else {
                panic!("copy")
            };
            super::super::gt::copy::tests::objects(s, d, operation).map_err(|_| AxError::Io)
        })
        .unwrap();
        for h in [src, dst] {
            let mut bytes = [0u8; 16384];
            object(&file, h)
                .unwrap()
                .backing
                .shared_pages()
                .unwrap()
                .read_bytes(0, &mut bytes)
                .unwrap();
            assert!(bytes.iter().all(|v| *v == 0x73));
        }
        file.intel_contexts.destroy(fresh).unwrap();
        assert_eq!(
            exec_with(&file, &copy, 0, |_, _, _| panic!("deleted context")),
            Err(AxError::NotFound)
        );
    }
    #[test]
    fn context_extension_and_copyout_fault_do_not_publish_a_partial_context() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 1024]));
        write_pod(
            &copy,
            0,
            &ContextExt {
                flags: 1,
                extensions: 512,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            dispatch(&file, &copy, CTX_EXT, 0),
            Err(AxError::InvalidInput)
        );
        struct Fail<'a>(&'a Image);
        impl UserCopy for Fail<'_> {
            fn read(&self, a: usize, b: &mut [MaybeUninit<u8>]) -> AxResult<()> {
                self.0.read(a, b)
            }
            fn write(&self, _: usize, _: &[u8]) -> AxResult<()> {
                Err(AxError::BadAddress)
            }
        }
        write_pod(&copy, 0, &ContextId::default()).unwrap();
        assert_eq!(
            context_create(&file, &Fail(&copy), 0, false),
            Err(AxError::BadAddress)
        );
        assert!(file.intel_contexts.state.lock().jobs.is_empty());
    }
    #[test]
    fn query_size_probe_payload_and_per_item_errors_follow_i915_wire_semantics() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let copy = Image(RefCell::new(vec![0; 2048]));
        let topo = || Ok(intel_gt::info::Topology { dss: 3, eus: 0xff });
        write_pod(
            &copy,
            0,
            &Query {
                count: 3,
                items: 256,
                flags: 0,
            },
        )
        .unwrap();
        for (i, id) in [1, 2, 99].into_iter().enumerate() {
            write_pod(
                &copy,
                256 + i * 24,
                &QueryItem {
                    id,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        query_with(&copy, 0, true, topo).unwrap();
        assert_eq!(read_pod::<QueryItem>(&copy, 256).unwrap().length, 30);
        assert_eq!(read_pod::<QueryItem>(&copy, 280).unwrap().length, 128);
        assert_eq!(read_pod::<QueryItem>(&copy, 304).unwrap().length, -22);
        let mut item: QueryItem = read_pod(&copy, 256).unwrap();
        item.data = 512;
        write_pod(&copy, 256, &item).unwrap();
        let mut engine: QueryItem = read_pod(&copy, 280).unwrap();
        engine.data = 1024;
        write_pod(&copy, 280, &engine).unwrap();
        query_with(&copy, 0, true, topo).unwrap();
        assert_eq!(
            read_pod::<[u8; 30]>(&copy, 512).unwrap(),
            topo().unwrap().wire()
        );
        assert_eq!(read_pod::<u32>(&copy, 1024).unwrap(), 2);
        assert_eq!(read_pod::<u16>(&copy, 1040).unwrap(), 0);
        assert_eq!(read_pod::<u16>(&copy, 1096).unwrap(), 1);
        // Nonzero engine header and undersized topology are per-item EINVAL,
        // not ioctl-wide success counts or a fabricated replacement payload.
        write_pod(&copy, 1024, &1u32).unwrap();
        item.length = 29;
        write_pod(&copy, 256, &item).unwrap();
        query_with(&copy, 0, true, topo).unwrap();
        assert_eq!(read_pod::<QueryItem>(&copy, 256).unwrap().length, -22);
        assert_eq!(read_pod::<QueryItem>(&copy, 280).unwrap().length, -22);
        item.length = 30;
        item.data = u64::MAX;
        write_pod(&copy, 256, &item).unwrap();
        query_with(&copy, 0, true, topo).unwrap();
        assert_eq!(read_pod::<QueryItem>(&copy, 256).unwrap().length, -14);
        item.id = 0;
        write_pod(&copy, 256, &item).unwrap();
        assert_eq!(query_with(&copy, 0, true, topo), Err(AxError::InvalidInput));
        write_pod(
            &copy,
            0,
            &Query {
                count: 0,
                items: u64::MAX,
                flags: 0,
            },
        )
        .unwrap();
        query_with(&copy, 0, true, topo).unwrap();
        write_pod(
            &copy,
            0,
            &Query {
                count: 2,
                items: 2024,
                flags: 0,
            },
        )
        .unwrap();
        write_pod(
            &copy,
            2024,
            &QueryItem {
                id: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(query_with(&copy, 0, true, topo), Err(AxError::BadAddress));
        assert_eq!(read_pod::<QueryItem>(&copy, 2024).unwrap().length, 30);
        for param in [20, 23, 35, 41, 43, 58] {
            assert_eq!(getparam_value(param, topo, || Ok(19_200_000)), Ok(0));
        }
        assert_eq!(getparam_value(50, topo, || Ok(0)), Ok(0));
        assert_eq!(
            getparam_value(51, topo, || Err(AxError::NoSuchDevice)),
            Err(AxError::NoSuchDevice)
        );
    }
    #[test]
    fn shared_vm_is_real_pinned_root_retained_across_alias_destroy_and_two_jobs() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 65536]));
        write_pod(&copy, 0, &VmControl::default()).unwrap();
        dispatch(&file, &copy, VM_CREATE, 0).unwrap();
        let id = read_pod::<VmControl>(&copy, 0).unwrap().id;
        let vm = file.intel_contexts.vm(id).unwrap();
        let root = vm.root();
        assert!(root != 0 && root.is_multiple_of(4096));
        let a = file.intel_contexts.create().unwrap();
        let b = file.intel_contexts.create().unwrap();
        file.intel_contexts.set_vm(a, id).unwrap();
        file.intel_contexts.set_vm(b, id).unwrap();
        file.intel_contexts.destroy_vm(id).unwrap();
        assert!(file.intel_contexts.vm(id).is_err());
        assert_eq!(file.intel_contexts.set_vm(b, id), Err(AxError::NotFound));
        for context in [a, b] {
            let (_, dst, ..) = prepare(&file, &copy);
            let mut request: Exec = read_pod(&copy, 0).unwrap();
            request.context = u64::from(context);
            super::super::gem_exec::exec_request(
                &file,
                &copy,
                request,
                crate::drm::fence::Fence::new(false),
                None,
                |src, dst, plan, active, _image| {
                    assert!(Arc::ptr_eq(&active, &vm));
                    assert_eq!(active.root(), root);
                    let Plan::Copy(operation) = plan else {
                        panic!("copy")
                    };
                    super::super::gt::copy::tests::objects_vm(src, dst, operation, active)
                        .map_err(|_| AxError::Io)
                },
            )
            .unwrap();
            let mut bytes = [0; 16384];
            object(&file, dst)
                .unwrap()
                .backing
                .shared_pages()
                .unwrap()
                .read_bytes(0, &mut bytes)
                .unwrap();
            assert!(bytes.iter().all(|b| *b == 0x73));
        }
        let replacement = file
            .intel_contexts
            .publish_vm(super::super::gt::copy::Vm::new().unwrap())
            .unwrap();
        assert_eq!(
            file.intel_contexts.set_vm(a, replacement),
            Err(AxError::InvalidInput)
        );
        file.intel_contexts.destroy(a).unwrap();
        file.intel_contexts.destroy(b).unwrap();
        assert_eq!(Arc::strong_count(&vm), 1);
    }
    #[test]
    fn vm_create_extension_and_getparam_preserve_source_alias_and_copyout_lifetimes() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 1024]));
        let original = file
            .intel_contexts
            .publish_vm(super::super::gt::copy::Vm::new().unwrap())
            .unwrap();
        write_pod(
            &copy,
            256,
            &SetparamExt {
                param: ContextParam {
                    param: 9,
                    value: u64::from(original),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        write_pod(
            &copy,
            0,
            &ContextExt {
                flags: 3,
                extensions: 256,
                ..Default::default()
            },
        )
        .unwrap();
        dispatch(&file, &copy, CTX_EXT, 0).unwrap();
        let id = read_pod::<ContextExt>(&copy, 0).unwrap().id;
        write_pod(
            &copy,
            0,
            &ContextParam {
                id,
                param: 9,
                ..Default::default()
            },
        )
        .unwrap();
        dispatch(&file, &copy, CTX_GETPARAM, 0).unwrap();
        let alias = read_pod::<ContextParam>(&copy, 0).unwrap().value as u32;
        assert_ne!(alias, original);
        assert!(Arc::ptr_eq(
            &file.intel_contexts.vm(alias).unwrap(),
            &file.intel_contexts.vm(original).unwrap()
        ));
        file.intel_contexts.destroy_vm(original).unwrap();
        assert!(file.intel_contexts.vm(alias).is_ok());
        struct Fail<'a>(&'a Image);
        impl UserCopy for Fail<'_> {
            fn read(&self, a: usize, b: &mut [MaybeUninit<u8>]) -> AxResult<()> {
                self.0.read(a, b)
            }
            fn write(&self, _: usize, _: &[u8]) -> AxResult<()> {
                Err(AxError::BadAddress)
            }
        }
        write_pod(&copy, 0, &VmControl::default()).unwrap();
        assert_eq!(
            dispatch(&file, &Fail(&copy), VM_CREATE, 0),
            Err(AxError::BadAddress)
        );
        assert_eq!(file.intel_contexts.state.lock().vms.len(), 1);
        write_pod(
            &copy,
            0,
            &ContextParam {
                id,
                param: 9,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            dispatch(&file, &Fail(&copy), CTX_GETPARAM, 0),
            Err(AxError::BadAddress)
        );
        assert_eq!(file.intel_contexts.state.lock().vms.len(), 1);
        write_pod(
            &copy,
            0,
            &VmControl {
                extensions: 1,
                id: alias,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            dispatch(&file, &copy, VM_DESTROY, 0),
            Err(AxError::InvalidInput)
        );
        assert!(file.intel_contexts.vm(alias).is_ok());
    }
    #[test]
    fn iris_engine_map_recoverable_vm_create_chain_selects_actual_bcs_slot() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 65536]));
        let id = file
            .intel_contexts
            .publish_vm(super::super::gt::copy::Vm::new().unwrap())
            .unwrap();
        write_pod(&copy, 2048, &0u64).unwrap();
        for (i, class) in [0, 0, 1].into_iter().enumerate() {
            write_pod(&copy, 2056 + i * 4, &Engine { class, instance: 0 }).unwrap();
        }
        write_pod(
            &copy,
            1024,
            &SetparamExt {
                next: 1080,
                param: ContextParam {
                    param: 10,
                    size: 20,
                    value: 2048,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        write_pod(
            &copy,
            1080,
            &SetparamExt {
                next: 1136,
                param: ContextParam {
                    param: 8,
                    value: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        write_pod(
            &copy,
            1136,
            &SetparamExt {
                param: ContextParam {
                    param: 9,
                    value: u64::from(id),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        write_pod(
            &copy,
            0,
            &ContextExt {
                flags: 1,
                extensions: 1024,
                ..Default::default()
            },
        )
        .unwrap();
        context_create_with(&file, &copy, 0, true, true).unwrap();
        let context = read_pod::<ContextExt>(&copy, 0).unwrap().id;
        let job = file.intel_contexts.lookup(context).unwrap();
        {
            let job = job.lock();
            assert_eq!(job.render_engine(0), Ok(true));
            assert_eq!(job.render_engine(1), Ok(true));
            assert_eq!(job.render_engine(2), Ok(false));
            assert_eq!(job.render_engine(3), Err(AxError::InvalidInput));
        }
        let (_, dst, ..) = prepare(&file, &copy);
        let mut exec: Exec = read_pod(&copy, 0).unwrap();
        exec.context = u64::from(context);
        exec.flags = (exec.flags & !0x3f) | 2;
        write_pod(&copy, 0, &exec).unwrap();
        exec_with(&file, &copy, 0, |src, dst, plan| {
            let Plan::Copy(operation) = plan else {
                panic!("engine slot interpreted as legacy render")
            };
            super::super::gt::copy::tests::objects(src, dst, operation).map_err(|_| AxError::Io)
        })
        .unwrap();
        let mut bytes = [0; 16384];
        object(&file, dst)
            .unwrap()
            .backing
            .shared_pages()
            .unwrap()
            .read_bytes(0, &mut bytes)
            .unwrap();
        assert!(bytes.iter().all(|b| *b == 0x73));
        write_pod(
            &copy,
            0,
            &ContextParam {
                id: context,
                param: 10,
                size: 20,
                value: 2048,
            },
        )
        .unwrap();
        assert_eq!(
            dispatch(&file, &copy, CTX_SETPARAM, 0),
            Err(AxError::InvalidInput)
        );
    }
    #[test]
    fn engine_map_source_errors_empty_invalid_slots_and_duplicate_assignment() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let file = file();
        let copy = Image(RefCell::new(vec![0; 1024]));
        let mut param = ContextParam {
            param: 10,
            size: 12,
            value: 256,
            ..Default::default()
        };
        write_pod(&copy, 256, &0u64).unwrap();
        write_pod(
            &copy,
            264,
            &Engine {
                class: 1,
                instance: 0,
            },
        )
        .unwrap();
        assert_eq!(engine_map(&copy, &param, false).unwrap(), [1]);
        write_pod(
            &copy,
            264,
            &Engine {
                class: 0,
                instance: 0,
            },
        )
        .unwrap();
        assert_eq!(engine_map(&copy, &param, false), Err(AxError::NotFound));
        write_pod(
            &copy,
            264,
            &Engine {
                class: 1,
                instance: 1,
            },
        )
        .unwrap();
        assert_eq!(engine_map(&copy, &param, true), Err(AxError::NotFound));
        write_pod(
            &copy,
            264,
            &Engine {
                class: u16::MAX,
                instance: u16::MAX,
            },
        )
        .unwrap();
        let mut job = JobContext::new();
        proto_param(&file, &copy, &mut job, &param, true).unwrap();
        assert_eq!(job.render_engine(0), Err(AxError::InvalidInput));
        assert_eq!(
            proto_param(&file, &copy, &mut job, &param, true),
            Err(AxError::InvalidInput)
        );
        write_pod(&copy, 256, &1u64).unwrap();
        assert_eq!(engine_map(&copy, &param, true), Err(AxError::InvalidInput));
        write_pod(&copy, 256, &0u64).unwrap();
        param.size = 8;
        let empty = engine_map(&copy, &param, true).unwrap();
        job.engines = Some(empty);
        assert_eq!(job.render_engine(0), Err(AxError::InvalidInput));
        for size in [0, 7, 9, 8 + 65 * 4] {
            param.size = size;
            assert_eq!(engine_map(&copy, &param, true), Err(AxError::InvalidInput));
        }
    }
    #[test]
    fn timestamp_ioctl_only_calls_source_whitelisted_wa_reader_and_preserves_errno() {
        let copy = Image(RefCell::new(vec![0; 32]));
        write_pod(
            &copy,
            0,
            &RegisterRead {
                offset: 0x2359,
                value: 0,
            },
        )
        .unwrap();
        register_read(&copy, 0, || Ok(0x1234_5678_9abc_def0)).unwrap();
        let r: RegisterRead = read_pod(&copy, 0).unwrap();
        assert_eq!(r.offset, 0x2359);
        assert_eq!(r.value, 0x1234_5678_9abc_def0);
        for offset in [0, 0x2358, 0x235a, 0x235c, 0x22000, 0x2359 + (1u64 << 32)] {
            write_pod(&copy, 0, &RegisterRead { offset, value: 7 }).unwrap();
            assert_eq!(
                register_read(&copy, 0, || panic!("arbitrary MMIO admitted")),
                Err(AxError::InvalidInput)
            );
        }
        write_pod(
            &copy,
            0,
            &RegisterRead {
                offset: 0x2359,
                value: 7,
            },
        )
        .unwrap();
        assert_eq!(
            register_read(&copy, 0, || Err(AxError::Io)),
            Err(AxError::Io)
        );
        assert_eq!(read_pod::<RegisterRead>(&copy, 0).unwrap().value, 7);
    }
}
