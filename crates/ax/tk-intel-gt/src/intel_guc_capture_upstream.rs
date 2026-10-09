// SPDX-License-Identifier: MIT
// Copyright © 2021-2022 Intel Corporation.
//! Linux 7.2.3 intel_guc_capture.c native linked-node extraction dependency
//! path. The existing source-translated CaptureBuffer supplies the byte reader;
//! this owner preserves preallocated-node reuse and does not allocate on G2H.
#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]
use core::{ffi::c_void, ptr};

use crate::{
    guc_capture::{CaptureBuffer, CaptureRegister},
    guc_log::{LogBufferState, LogStats, intel_guc_check_log_buf_overflow},
    intel_context_types_upstream::IntelContext,
    intel_engine_cs_upstream::ListHead,
    intel_engine_types_upstream::IntelEngineCs,
    intel_gt_types_upstream::IntelGt,
    intel_guc_types_upstream::IntelGuc,
    linux::{
        list::{INIT_LIST_HEAD, list_add_tail, list_del, list_empty},
        memory::{kfree, kzalloc_obj, kzalloc_objs},
    },
    linux_config::{EIO, ENOMEM},
};
const ENODATA: i32 = 61;
#[repr(C)]
pub struct CaptureAdsCache {
    pub is_valid: bool,
    pub ptr: *mut c_void,
    pub size: usize,
    pub status: i32,
}
#[repr(C)]
pub struct IntelGucStateCapture {
    pub reglists: *const c_void,
    pub extlists: *mut c_void,
    pub ads_cache: [[[CaptureAdsCache; 16]; 3]; 2],
    pub ads_null_cache: *mut c_void,
    pub cachelist: ListHead,
    pub max_mmio_per_node: i32,
    pub outlist: ListHead,
}
#[repr(C)]
pub struct RegInfo {
    pub vfid: u32,
    pub num_regs: u32,
    pub regs: *mut CaptureRegister,
}
#[repr(C)]
pub struct ParsedOutput {
    pub link: ListHead,
    pub is_partial: bool,
    pub eng_class: u32,
    pub eng_inst: u32,
    pub guc_id: u32,
    pub lrca: u32,
    pub reginfo: [RegInfo; 3],
}
const _: [(); 3136] = [(); core::mem::size_of::<IntelGucStateCapture>()];
const _: [(); 88] = [(); core::mem::size_of::<ParsedOutput>()];
// upstream: intel_guc_capture.c guc_capture_delete_one_node()
unsafe fn guc_capture_delete_one_node(_guc: *mut IntelGuc, node: *mut ParsedOutput) {
    for list in &(*node).reginfo {
        kfree(list.regs);
    }
    list_del(&mut (*node).link);
    kfree(node);
}
// upstream: intel_guc_capture.c guc_capture_delete_prealloc_nodes()
unsafe fn guc_capture_delete_prealloc_nodes(guc: *mut IntelGuc) {
    for head in [
        &mut (*(*guc).capture).outlist as *mut ListHead,
        &mut (*(*guc).capture).cachelist as *mut ListHead,
    ] {
        while !list_empty(&*head) {
            guc_capture_delete_one_node(guc, (*head).next.cast());
        }
    }
}
// upstream: intel_guc_capture.c guc_capture_add_node_to_list()
unsafe fn guc_capture_add_node_to_list(node: *mut ParsedOutput, list: *mut ListHead) {
    list_add_tail(&mut (*node).link, list);
}
// upstream: intel_guc_capture.c guc_capture_add_node_to_outlist()
unsafe fn guc_capture_add_node_to_outlist(gc: *mut IntelGucStateCapture, node: *mut ParsedOutput) {
    guc_capture_add_node_to_list(node, &mut (*gc).outlist);
}
// upstream: intel_guc_capture.c guc_capture_add_node_to_cachelist()
pub unsafe fn guc_capture_add_node_to_cachelist(
    gc: *mut IntelGucStateCapture,
    node: *mut ParsedOutput,
) {
    guc_capture_add_node_to_list(node, &mut (*gc).cachelist);
}
// upstream: intel_guc_capture.c guc_capture_init_node()
unsafe fn guc_capture_init_node(guc: *mut IntelGuc, node: *mut ParsedOutput) {
    let mut tmp = [ptr::null_mut(); 3];
    for i in 0..3 {
        tmp[i] = (*node).reginfo[i].regs;
        ptr::write_bytes(tmp[i], 0, (*(*guc).capture).max_mmio_per_node as usize);
    }
    ptr::write_bytes(node, 0, 1);
    for i in 0..3 {
        (*node).reginfo[i].regs = tmp[i];
    }
    INIT_LIST_HEAD(&mut (*node).link);
}
// upstream: intel_guc_capture.c guc_capture_get_prealloc_node()
unsafe fn guc_capture_get_prealloc_node(guc: *mut IntelGuc) -> *mut ParsedOutput {
    let gc = (*guc).capture;
    let mut found: *mut ParsedOutput = ptr::null_mut();
    if !list_empty(&(*gc).cachelist) {
        found = (*gc).cachelist.next.cast();
        list_del(&mut (*found).link);
    } else {
        let head = &mut (*gc).outlist as *mut ListHead;
        let mut n = (*head).next;
        while n != head {
            found = n.cast();
            n = (*n).next;
        }
        if !found.is_null() {
            list_del(&mut (*found).link);
        }
    }
    if !found.is_null() {
        guc_capture_init_node(guc, found);
    }
    found
}
// upstream: intel_guc_capture.c guc_capture_alloc_one_node()
unsafe fn guc_capture_alloc_one_node(guc: *mut IntelGuc) -> *mut ParsedOutput {
    let new = kzalloc_obj::<ParsedOutput>();
    if new.is_null() {
        return new;
    }
    for i in 0..3 {
        (*new).reginfo[i].regs =
            kzalloc_objs::<CaptureRegister, _>((*(*guc).capture).max_mmio_per_node as usize);
        if (*new).reginfo[i].regs.is_null() {
            for j in (0..i).rev() {
                kfree((*new).reginfo[j].regs);
            }
            kfree(new);
            return ptr::null_mut();
        }
    }
    guc_capture_init_node(guc, new);
    new
}
// upstream: intel_guc_capture.c guc_capture_clone_node()
unsafe fn guc_capture_clone_node(
    guc: *mut IntelGuc,
    original: *mut ParsedOutput,
    keep: u32,
) -> *mut ParsedOutput {
    let new = guc_capture_get_prealloc_node(guc);
    if new.is_null() {
        return new;
    }
    if original.is_null() {
        return new;
    }
    (*new).is_partial = (*original).is_partial;
    for i in 0..3 {
        if keep & (1 << i) != 0 {
            GEM_BUG_ON!(
                (*original).reginfo[i].num_regs > (*(*guc).capture).max_mmio_per_node as u32
            );
            ptr::copy_nonoverlapping(
                (*original).reginfo[i].regs,
                (*new).reginfo[i].regs,
                (*original).reginfo[i].num_regs as usize,
            );
            (*new).reginfo[i].num_regs = (*original).reginfo[i].num_regs;
            (*new).reginfo[i].vfid = (*original).reginfo[i].vfid;
            if i == 1 {
                (*new).eng_class = (*original).eng_class;
            } else if i == 2 {
                (*new).eng_inst = (*original).eng_inst;
                (*new).guc_id = (*original).guc_id;
                (*new).lrca = (*original).lrca;
            }
        }
    }
    new
}
// upstream: intel_guc_capture.c __guc_capture_create_prealloc_nodes()
unsafe fn __guc_capture_create_prealloc_nodes(guc: *mut IntelGuc) {
    for _ in 0..3 * 16 * 32 {
        let node = guc_capture_alloc_one_node(guc);
        if node.is_null() {
            axlog::warn!("Register capture pre-alloc-cache failure");
            return;
        }
        guc_capture_add_node_to_cachelist((*guc).capture, node);
    }
}
/// Explicit mechanism initialization; runtime ADS/IRQ wiring remains off. The
/// owner supplies the max register count measured while building its ADS lists.
pub unsafe fn init_native_capture(guc: *mut IntelGuc, max_registers: i32) -> i32 {
    if max_registers <= 0 || !(*guc).capture.is_null() {
        return -crate::linux_config::EINVAL;
    }
    let gc = kzalloc_obj::<IntelGucStateCapture>();
    if gc.is_null() {
        return -ENOMEM;
    }
    INIT_LIST_HEAD(&mut (*gc).cachelist);
    INIT_LIST_HEAD(&mut (*gc).outlist);
    (*gc).max_mmio_per_node = max_registers;
    (*guc).capture = gc;
    __guc_capture_create_prealloc_nodes(guc);
    0
}
// upstream: intel_guc_capture.c guc_capture_extract_reglists()
unsafe fn guc_capture_extract_reglists(guc: *mut IntelGuc, buf: &mut CaptureBuffer<'_>) -> i32 {
    let mut node: *mut ParsedOutput = ptr::null_mut();
    let mut ret = 0;
    let i = buf.count();
    if i == 0 {
        return -ENODATA;
    }
    if i % 4 != 0 {
        axlog::warn!("Got mis-aligned register capture entries");
        return -EIO;
    }
    let ghdr = match buf.read_words::<2>() {
        Ok(v) => v,
        Err(_) => return -EIO,
    };
    let partial = (ghdr[1] >> 8) & 0xff != 0;
    let mut lists = (ghdr[1] & 0xff) as i32;
    while lists > 0 {
        lists -= 1;
        let hdr = match buf.read_words::<5>() {
            Ok(v) => v,
            Err(_) => {
                ret = -EIO;
                break;
            }
        };
        let datatype = (hdr[1] & 0xf) as usize;
        if datatype > 2 {
            let mut regs = (hdr[4] & 0x3ff) as i32;
            while regs > 0 {
                regs -= 1;
                if buf.read_words::<4>().is_err() {
                    ret = -EIO;
                    break;
                }
            }
            continue;
        } else if !node.is_null() {
            if datatype == 0 {
                guc_capture_add_node_to_outlist((*guc).capture, node);
                node = ptr::null_mut();
            } else if datatype == 1 && (*node).reginfo[1].num_regs != 0 {
                guc_capture_add_node_to_outlist((*guc).capture, node);
                node = guc_capture_clone_node(guc, node, 1);
            } else if datatype == 2 && (*node).reginfo[2].num_regs != 0 {
                guc_capture_add_node_to_outlist((*guc).capture, node);
                node = guc_capture_clone_node(guc, node, 3);
            }
        }
        if node.is_null() {
            node = guc_capture_get_prealloc_node(guc);
            if node.is_null() {
                ret = -ENOMEM;
                break;
            }
            if datatype != 0 {
                axlog::debug!("Register capture missing global dump: {datatype:08x}");
            }
        }
        (*node).is_partial = partial;
        (*node).reginfo[datatype].vfid = hdr[0] & 0xff;
        match datatype {
            2 => {
                (*node).eng_class = (hdr[1] >> 4) & 0xf;
                (*node).eng_inst = (hdr[1] >> 8) & 0xf;
                (*node).lrca = hdr[2];
                (*node).guc_id = hdr[3];
            }
            1 => (*node).eng_class = (hdr[1] >> 4) & 0xf,
            _ => {}
        }
        let mut numregs = hdr[4] & 0x3ff;
        if numregs > (*(*guc).capture).max_mmio_per_node as u32 {
            axlog::debug!("Register capture list extraction clipped by prealloc");
            numregs = (*(*guc).capture).max_mmio_per_node as u32;
        }
        (*node).reginfo[datatype].num_regs = numregs;
        for i in 0..numregs {
            let v = match buf.read_words::<4>() {
                Ok(v) => v,
                Err(_) => {
                    ret = -EIO;
                    break;
                }
            };
            (*node).reginfo[datatype]
                .regs
                .add(i as usize)
                .write(CaptureRegister {
                    offset: v[0],
                    value: v[1],
                    flags: v[2],
                    mask: v[3],
                });
        }
    }
    if !node.is_null() {
        for i in 0..3 {
            if !(*node).reginfo[i].regs.is_null() {
                guc_capture_add_node_to_outlist((*guc).capture, node);
                node = ptr::null_mut();
                break;
            }
        }
        if !node.is_null() {
            guc_capture_add_node_to_cachelist((*guc).capture, node);
        }
    }
    ret
}
// upstream: intel_guc_capture.c __guc_capture_flushlog_complete()
unsafe fn __guc_capture_flushlog_complete(guc: *mut IntelGuc) -> i32 {
    crate::guc_submission_upstream::intel_guc_send_nb(&mut *guc, &[0x30, 2], 0)
}
// upstream: intel_guc_capture.c __guc_capture_process_output()
unsafe fn __guc_capture_process_output(guc: *mut IntelGuc) {
    let gt = crate::intel_gt_api_upstream::guc_to_gt(guc);
    let state = (*guc).log.buf_addr.cast::<LogBufferState>().add(2);
    assert!(
        !(*guc).log.buf_addr.is_null(),
        "capture log has no mapped buffer owner"
    );
    let local = state.read_unaligned();
    let size = (*guc).log.sizes[2].bytes as usize;
    let source = (*guc)
        .log
        .buf_addr
        .cast::<u8>()
        .add(4096 + (*guc).log.sizes[0].bytes as usize + (*guc).log.sizes[1].bytes as usize);
    let mut rd = local.read_ptr as usize;
    let mut wr = local.sampled_write_ptr as usize;
    let full = (local.flags >> 1) & 0xf;
    (*guc).log.stats[2].flush = (*guc).log.stats[2].flush.wrapping_add(local.flags & 1);
    let s = &mut (*guc).log.stats[2];
    let mut stats = LogStats {
        sampled_overflow: s.sampled_overflow,
        overflow: s.overflow,
        flush: s.flush,
    };
    let overflow = intel_guc_check_log_buf_overflow(&mut stats, full);
    s.sampled_overflow = stats.sampled_overflow;
    s.overflow = stats.overflow;
    if overflow {
        rd = 0;
        wr = size;
    } else if rd > size || wr > size {
        axlog::error!("Register capture buffer in invalid state: read={rd:x}, size={size:x}");
        rd = 0;
        wr = size;
    }
    if !(*gt).uc.reset_in_progress {
        let mut buf = CaptureBuffer::new(core::slice::from_raw_parts(source, size), rd, wr)
            .expect("invalid configured capture buffer");
        loop {
            if guc_capture_extract_reglists(guc, &mut buf) < 0 {
                break;
            }
        }
    }
    ptr::addr_of_mut!((*state).read_ptr).write_unaligned(wr as u32);
    let flags = ptr::addr_of!((*state).flags).read_unaligned();
    ptr::addr_of_mut!((*state).flags).write_unaligned(flags & !1);
    __guc_capture_flushlog_complete(guc);
}
// upstream: intel_guc_capture.c intel_guc_capture_is_matching_engine()
pub unsafe fn intel_guc_capture_is_matching_engine(
    gt: *mut IntelGt,
    ce: *mut IntelContext,
    engine: *mut IntelEngineCs,
) -> bool {
    if gt.is_null() || ce.is_null() || engine.is_null() {
        return false;
    }
    let guc = crate::intel_gt_api_upstream::gt_to_guc(gt);
    if (*guc).capture.is_null() {
        return false;
    }
    let head = &(*(*guc).capture).outlist as *const ListHead as *mut ListHead;
    let mut node = (*head).next;
    while node != head {
        let n = node.cast::<ParsedOutput>();
        if (*n).eng_inst == ((*engine).guc_id as u32 >> 3) & 0xf
            && (*n).eng_class == (*engine).guc_id as u32 & 7
            && (*n).guc_id == (*ce).guc_id.id as u32
            && ((*n).lrca & 0xfffff000) == ((&(*ce).lrc).lrca & 0xfffff000)
        {
            return true;
        }
        node = (*node).next;
    }
    false
}
// upstream: intel_guc_capture.c intel_guc_capture_process()
pub unsafe fn intel_guc_capture_process(guc: *mut IntelGuc) {
    if !(*guc).capture.is_null() {
        __guc_capture_process_output(guc);
    }
}
