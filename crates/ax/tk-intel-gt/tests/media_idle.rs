// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod support;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    process::Command,
};

use tk_intel_gt::{Error, GtIo, uncore};
struct Io {
    words: RefCell<BTreeMap<u32, u32>>,
    writes: Cell<usize>,
    fail: usize,
    clock: Cell<u64>,
}
impl Io {
    fn new(fail: usize) -> Self {
        let mut words = BTreeMap::new();
        for (_, request, ack, mode) in uncore::MEDIA {
            words.insert(request, 1 << 12);
            words.insert(ack, 0);
            words.insert(mode, 1 << 9);
            words.insert(mode - 0x9c + 0x34, 0);
            words.insert(mode - 0x9c + 0x30, 0);
        }
        Self {
            words: RefCell::new(words),
            writes: Cell::new(0),
            fail,
            clock: Cell::new(0),
        }
    }
}
impl GtIo for Io {
    fn read(&self, r: u32) -> Result<u32, Error> {
        self.words
            .borrow()
            .get(&r)
            .copied()
            .ok_or(Error::Unavailable(r))
    }
    fn write(&self, r: u32, v: u32) -> Result<(), Error> {
        self.writes.set(self.writes.get() + 1);
        let mut words = self.words.borrow_mut();
        let old = words.get(&r).copied().ok_or(Error::Unavailable(r))?;
        let value = (old & !(v >> 16)) | (v & (v >> 16));
        words.insert(r, value);
        let (_, _, ack, _) = uncore::MEDIA.iter().find(|(_, req, ..)| *req == r).unwrap();
        words.insert(*ack, value & 0x8001);
        if self.writes.get() == self.fail {
            Err(Error::Unavailable(r))
        } else {
            Ok(())
        }
    }
    fn now_us(&self) -> u64 {
        self.clock.get()
    }
    fn delay_us(&self, n: u32) {
        self.clock.set(self.clock.get() + u64::from(n));
    }
}
#[test]
fn fused_media_domains_require_owned_ack_and_real_idle_before_shared_policy() {
    for disabled in 0..8u8 {
        let fuse = (if disabled & 1 != 0 { 1 } else { 0 })
            | (if disabled & 2 != 0 { 4 } else { 0 })
            | (if disabled & 4 != 0 { 1 << 16 } else { 0 });
        let present = uncore::media_mask(fuse);
        assert_eq!(present, (!disabled) & 7);
        let io = Io::new(0);
        let mut owned = 0;
        if present != 0 {
            assert!(uncore::idle_media(&io, present, 0).is_err());
        }
        for index in 0..3 {
            if present & (1 << index) != 0 {
                uncore::acquire_media(&io, index).unwrap();
                owned |= 1 << index;
            }
        }
        uncore::idle_media(&io, present, owned).unwrap();
        for index in 0..3 {
            if present & (1 << index) != 0 {
                let (_, _, ack, mode) = uncore::MEDIA[index];
                io.words.borrow_mut().insert(mode, 0);
                assert!(uncore::idle_media(&io, present, owned).is_err());
                io.words.borrow_mut().insert(mode, 1 << 9);
                io.words.borrow_mut().insert(ack, 0);
                assert!(uncore::idle_media(&io, present, owned).is_err());
                io.words.borrow_mut().insert(ack, 1);
            }
        }
        assert!(uncore::idle_media(&io, 0x80, owned).is_err());
    }
}
#[test]
fn landed_media_wake_fault_never_returns_ownership_or_resets_a_media_engine() {
    for domain in 0..3 {
        for prefix in 1..=2 {
            let io = Io::new(prefix);
            assert!(uncore::acquire_media(&io, domain).is_err());
            let (_, request, ack, _) = uncore::MEDIA[domain];
            assert_eq!(io.words.borrow()[&request], 1 << 12);
            assert_eq!(io.words.borrow()[&ack], 0);
        }
    }
}
#[test]
#[ignore = "requires local Linux7.2.3/GCC"]
fn selected_n305_media_domains_match_source_register_definitions() {
    let root = support::reference();
    let regs = support::read(&root, "gt/intel_gt_regs.h");
    let reg = support::read(&root, "i915_reg.h");
    let defs = support::defines(
        &regs,
        &[
            "FORCEWAKE_MEDIA_VDBOX_GEN11",
            "FORCEWAKE_ACK_MEDIA_VDBOX_GEN11",
            "FORCEWAKE_MEDIA_VEBOX_GEN11",
            "FORCEWAKE_ACK_MEDIA_VEBOX_GEN11",
        ],
    ) + &support::defines(
        &reg,
        &[
            "GEN11_BSD_RING_BASE",
            "GEN11_BSD3_RING_BASE",
            "GEN11_VEBOX_RING_BASE",
        ],
    );
    let code = format!(
        "#include <stdio.h>\n#define _MMIO(r) (r)\n{defs}\nint main(void){{printf(\"%x %x \
         %x\\n\",FORCEWAKE_MEDIA_VDBOX_GEN11(0),FORCEWAKE_ACK_MEDIA_VDBOX_GEN11(0),\
         GEN11_BSD_RING_BASE+0x9c);printf(\"%x %x \
         %x\\n\",FORCEWAKE_MEDIA_VDBOX_GEN11(2),FORCEWAKE_ACK_MEDIA_VDBOX_GEN11(2),\
         GEN11_BSD3_RING_BASE+0x9c);printf(\"%x %x \
         %x\\n\",FORCEWAKE_MEDIA_VEBOX_GEN11(0),FORCEWAKE_ACK_MEDIA_VEBOX_GEN11(0),\
         GEN11_VEBOX_RING_BASE+0x9c);}}"
    );
    let oracle = support::compile(&code, "media-domains");
    let output = Command::new(&oracle.executable).output().unwrap();
    assert!(output.status.success());
    for (line, (_, request, ack, mode)) in String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .zip(uncore::MEDIA)
    {
        let values: Vec<_> = line
            .split_whitespace()
            .map(|v| u32::from_str_radix(v, 16).unwrap())
            .collect();
        assert_eq!(values, [request, ack, mode]);
    }
}

#[test]
#[ignore = "requires local Linux7.2.3/GCC"]
fn ring_idle_requires_source_head_tail_and_parser_idle_checks() {
    let root = support::reference();
    let source = support::read(&root, "gt/intel_engine_cs.c");
    let regs = support::read(&root, "gt/intel_engine_regs.h");
    let function = support::function(&source, "static bool ring_is_idle(");
    let defs = support::defines(
        &regs,
        &[
            "RING_HEAD",
            "RING_TAIL",
            "HEAD_ADDR",
            "TAIL_ADDR",
            "RING_MI_MODE",
            "MODE_IDLE",
        ],
    );
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#define _MMIO(r) (r)
#define REG_BIT(n) (1U<<(n))
#define I915_SELFTEST_ONLY(x) 0
#define GRAPHICS_VER(x) 12
struct intel_engine_cs{unsigned mmio_base;uint32_t head,tail,mode;};
static bool intel_engine_pm_get_if_awake(struct intel_engine_cs*e){return true;}
static void intel_engine_pm_put(struct intel_engine_cs*e){}
static uint32_t read_ring(struct intel_engine_cs*e,unsigned r){switch(r-e->mmio_base){case 0x34:return e->head;case 0x30:return e->tail;case 0x9c:return e->mode;default:__builtin_trap();}}
#define ENGINE_READ(e,r) read_ring(e,r(e->mmio_base))
"#;
    let main = r#"int main(void){for(unsigned h=0;h<4;h++)for(unsigned t=0;t<4;t++)for(unsigned idle=0;idle<2;idle++){struct intel_engine_cs e={.mmio_base=0x1c0000,.head=h*4,.tail=t*8,.mode=idle<<9};printf("%u %u %u %u\n",e.head,e.tail,e.mode,ring_is_idle(&e));}}"#;
    let oracle = support::compile(&[prefix, &defs, &function, main].join("\n"), "ring-idle");
    let output = Command::new(&oracle.executable).output().unwrap();
    assert!(output.status.success());
    for line in String::from_utf8(output.stdout).unwrap().lines() {
        let values: Vec<u32> = line
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect();
        let io = Io::new(0);
        io.words.borrow_mut().insert(0x1c0034, values[0]);
        io.words.borrow_mut().insert(0x1c0030, values[1]);
        io.words.borrow_mut().insert(0x1c009c, values[2]);
        assert_eq!(uncore::ring_idle(&io, 0x1c0000).unwrap(), values[3] != 0);
    }
    assert!(uncore::ring_idle(&Io::new(0), u32::MAX).is_err());
}
