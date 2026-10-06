// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. Repository MIT license.
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
// complete private hardware image/VM; the small immutable-job ABI does not
// promise arbitrary retained graphics state or shared user VM extensions.
pub(crate) struct Contexts {
    default: Arc<axsync::Mutex<()>>,
    state: spin::Mutex<ContextState>,
}
struct ContextState {
    next: u32,
    jobs: BTreeMap<u32, Arc<axsync::Mutex<()>>>,
}
impl Contexts {
    pub(crate) fn new() -> Self {
        Self {
            default: Arc::new(axsync::Mutex::new(())),
            state: spin::Mutex::new(ContextState {
                next: 1,
                jobs: BTreeMap::new(),
            }),
        }
    }
    fn create(&self) -> AxResult<u32> {
        let gate = Arc::try_new(axsync::Mutex::new(())).map_err(|_| AxError::NoMemory)?;
        let mut state = self.state.lock();
        if state.jobs.len() >= 256 {
            return Err(AxError::NoMemory);
        }
        let id = state.next;
        state.next = id.checked_add(1).ok_or(AxError::NoMemory)?;
        state.jobs.insert(id, gate);
        Ok(id)
    }
    pub(super) fn lookup(&self, id: u32) -> AxResult<Arc<axsync::Mutex<()>>> {
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
const GETPARAM: u32 = command::<Getparam>(6, 3);
const QUERY: u32 = command::<Query>(0x39, 3);
// Independently compiled Linux7.2.3 x86_64 header facts.
const _: () = {
    assert!(size_of::<ContextId>() == 8 && size_of::<ContextExt>() == 16);
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

fn context_create(
    file: &DrmFile,
    copy: &impl UserCopy,
    arg: usize,
    extended: bool,
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
    if request.flags != 0 || request.extensions != 0 {
        return Err(AxError::InvalidInput);
    }
    request.id = file.intel_contexts.create()?;
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
        5 | 9 | 11 | 19 | 24 | 25 | 37 | 49 | 55 => 1,
        40 => 4, // WB mmap-offset only; no legacy GTT aperture mmap.
        33 => topology()?.dss.count_ones() as i32,
        34 => topology()?.eu_total() as i32,
        46 => {
            topology()?;
            1
        }
        47 => i32::from(topology()?.dss),
        50 => 0, // Source requires captured engine default_state; not yet present.
        51 => i32::try_from(clock()?).map_err(|_| AxError::InvalidInput)?,
        6..=8
        | 10
        | 12..=18
        | 20..=23
        | 26..=31
        | 35
        | 36
        | 38
        | 39
        | 41..=45
        | 48
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
            let _context = file.intel_contexts.lookup(r.id)?;
            if r.size != 0 || r.param != 3 {
                return Err(AxError::InvalidInput);
            }
            r.value = 0x40000;
            write_pod(copy, arg, &r)?;
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
        for param in [20, 23, 35, 41, 43, 44, 58] {
            assert_eq!(getparam_value(param, topo, || Ok(19_200_000)), Ok(0));
        }
        assert_eq!(getparam_value(50, topo, || Ok(0)), Ok(0));
        assert_eq!(
            getparam_value(51, topo, || Err(AxError::NoSuchDevice)),
            Err(AxError::NoSuchDevice)
        );
    }
}
