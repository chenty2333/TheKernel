//! BPF file descriptor types (BpfMapFd, BpfProgFd).
//!
//! These are thin wrappers that implement `FileLike + Pollable` so BPF objects
//! can be managed through the standard fd table and used with close(), dup(), etc.

use alloc::{borrow::Cow, sync::Arc};
use core::task::Context;

use axerrno::AxResult;
use axpoll::{IoEvents, Pollable};

use crate::{
    bpf::{defs::BPF_OBJ_NAME_LEN, map::BpfMap, prog::BpfProgram},
    file::{FileLike, Kstat, anon_inode_stat},
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
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:bpf-map".into())
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        // The map fd has no blocking file operation; the OFD owns the flag.
        Ok(())
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
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:bpf-prog".into())
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        // The program fd has no blocking file operation; the OFD owns the flag.
        Ok(())
    }
}

impl Pollable for BpfProgFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}
