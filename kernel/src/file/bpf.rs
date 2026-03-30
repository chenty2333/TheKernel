//! BPF file descriptor types (BpfMapFd, BpfProgFd).
//!
//! These are thin wrappers that implement `FileLike + Pollable` so BPF objects
//! can be managed through the standard fd table and used with close(), dup(), etc.

use alloc::{borrow::Cow, sync::Arc};
use core::task::Context;

use axpoll::{IoEvents, Pollable};

use crate::{
    bpf::{defs::BPF_OBJ_NAME_LEN, map::BpfMap, prog::BpfProgram},
    file::FileLike,
};

// ---------------------------------------------------------------------------
// BpfMapFd
// ---------------------------------------------------------------------------

pub struct BpfMapFd {
    pub map: Arc<dyn BpfMap>,
    pub map_id: u32,
    pub name: [u8; BPF_OBJ_NAME_LEN],
}

impl BpfMapFd {
    pub fn new(map: Arc<dyn BpfMap>, map_id: u32, name: [u8; BPF_OBJ_NAME_LEN]) -> Self {
        Self { map, map_id, name }
    }
}

impl FileLike for BpfMapFd {
    fn path(&self) -> Cow<'_, str> {
        "anon_inode:bpf-map".into()
    }
}

impl Pollable for BpfMapFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

// ---------------------------------------------------------------------------
// BpfProgFd
// ---------------------------------------------------------------------------

pub struct BpfProgFd {
    pub prog: Arc<BpfProgram>,
}

impl BpfProgFd {
    pub fn new(prog: Arc<BpfProgram>) -> Self {
        Self { prog }
    }
}

impl FileLike for BpfProgFd {
    fn path(&self) -> Cow<'_, str> {
        "anon_inode:bpf-prog".into()
    }
}

impl Pollable for BpfProgFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}
