//! Host-only controller: emulate DMA and completions, not just MMIO flags.
extern crate std;
use alloc::vec;
use std::{
    alloc::{Layout, alloc_zeroed, dealloc},
    collections::BTreeMap,
};

use axdriver_block::{BlockDriverOps, DevError};

use super::*;
use crate::{
    desc::{Command, prps},
    regs::{Bus, *},
};
struct Host;
// SAFETY: page-aligned host allocation serves as fake physical address.
unsafe impl Hal for Host {
    fn allocate(pages: usize) -> Option<(u64, NonNull<u8>)> {
        // SAFETY: nonzero, page-aligned layout.
        let pointer = NonNull::new(unsafe {
            alloc_zeroed(Layout::from_size_align(pages * 4096, 4096).unwrap())
        })?;
        Some((pointer.as_ptr() as u64, pointer))
    }
    unsafe fn release(_address: u64, pointer: NonNull<u8>, pages: usize) {
        // SAFETY: fake DMA retired and layout matches allocate.
        unsafe {
            dealloc(
                pointer.as_ptr(),
                Layout::from_size_align(pages * 4096, 4096).unwrap(),
            );
        }
    }
}
struct Fake {
    registers: BTreeMap<usize, u32>,
    queues: BTreeMap<u16, (u64, u64, usize, usize, u16)>,
    disk: alloc::vec::Vec<u8>,
    commands: alloc::vec::Vec<(u16, u8)>,
    mdts: u8,
    namespace: [u8; 4096],
    timeout: bool,
    irq: bool,
    delay_io: bool,
    pending: Option<u16>,
    delivered: std::sync::Arc<core::sync::atomic::AtomicU64>,
}
impl Fake {
    fn new() -> Self {
        let cap = 31u64 | (1 << 37) | (1 << 24);
        let mut registers = BTreeMap::new();
        registers.insert(CAP, cap as u32);
        registers.insert(CAP + 4, (cap >> 32) as u32);
        let mut namespace = [0; 4096];
        namespace[..8].copy_from_slice(&2048u64.to_le_bytes());
        namespace[130] = 9;
        Self {
            mdts: 5,
            namespace,
            registers,
            queues: BTreeMap::new(),
            disk: vec![0; 1024 * 1024],
            commands: vec![],
            timeout: false,
            irq: false,
            delay_io: false,
            pending: None,
            delivered: std::sync::Arc::new(core::sync::atomic::AtomicU64::new(0)),
        }
    }
    fn addr(&self, register: usize) -> u64 {
        u64::from(*self.registers.get(&register).unwrap_or(&0))
            | (u64::from(*self.registers.get(&(register + 4)).unwrap_or(&0)) << 32)
    }
    fn execute(&mut self, id: u16) {
        let (sq, cq, tail, head, phase) = self.queues[&id];
        // SAFETY: fake controller was given a valid owned SQ; one entry per submit.
        let c = unsafe { (sq as *const Command).add(tail).read() }.0;
        let opcode = c[0] as u8;
        self.commands.push((id, opcode));
        let pointer = u64::from(c[6]) | (u64::from(c[7]) << 32);
        let mut result = 0;
        if id == 0 {
            match opcode {
                6 => {
                    // SAFETY: Identify PRP addresses an allocated 4-KiB page.
                    let out = unsafe { core::slice::from_raw_parts_mut(pointer as *mut u8, 4096) };
                    out.fill(0);
                    match c[10] {
                        1 => out[77] = self.mdts,
                        2 => out[..4].copy_from_slice(&1u32.to_le_bytes()),
                        _ => out.copy_from_slice(&self.namespace),
                    }
                }
                9 => result = 0x10001,
                5 => {
                    assert_eq!(c[11] & 2 != 0, self.irq);
                    assert_eq!(c[11] >> 16, 0);
                    self.queues.insert(c[10] as u16, (0, pointer, 0, 0, 1));
                }
                1 => {
                    self.queues.get_mut(&(c[10] as u16)).unwrap().0 = pointer;
                }
                _ => panic!("unexpected admin opcode"),
            }
        } else if opcode != 0 {
            let lba = u64::from(c[10]) | (u64::from(c[11]) << 32);
            let length = ((c[12] & 65535) + 1) as usize * 512;
            if (1..=4).contains(&self.mdts) {
                assert!(
                    length <= 4096usize << self.mdts,
                    "command exceeded controller MDTS"
                );
            }
            let second = u64::from(c[8]) | (u64::from(c[9]) << 32);
            for offset in (0..length).step_by(4096) {
                let address = if offset == 0 {
                    pointer
                } else if length <= 8192 {
                    second
                } else {
                    // SAFETY: PRP list covers all pages in this bounded transfer.
                    unsafe { (second as *const u64).add(offset / 4096 - 1).read() }
                };
                let amount = (length - offset).min(4096);
                // SAFETY: all PRPs are pages from the driver's bounce allocation.
                let page = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, amount) };
                let disk = &mut self.disk
                    [lba as usize * 512 + offset..lba as usize * 512 + offset + amount];
                if opcode == 1 {
                    disk.copy_from_slice(page);
                } else {
                    page.copy_from_slice(disk);
                }
            }
        }
        // SAFETY: CQ entry is in the fake device's allocated queue.
        unsafe {
            let out = (cq as *mut u32).add(head * 4);
            out.write(result);
            out.add(2).write(u32::from(id) << 16);
            out.add(3).write((c[0] >> 16) | (u32::from(phase) << 16));
        }
        if self.irq {
            self.delivered
                .fetch_add(1, core::sync::atomic::Ordering::Release);
        }
        let next = (head + 1) % 32;
        self.queues.insert(
            id,
            (
                sq,
                cq,
                (tail + 1) % 32,
                next,
                if next == 0 { phase ^ 1 } else { phase },
            ),
        );
    }
}
impl Bus for Fake {
    fn read32(&mut self, offset: usize) -> u32 {
        *self.registers.get(&offset).unwrap_or(&0)
    }
    fn write32(&mut self, offset: usize, value: u32) {
        self.registers.insert(offset, value);
        if offset == CC {
            self.registers.insert(CSTS, value & 1);
            if value & 1 != 0 {
                self.queues
                    .insert(0, (self.addr(ASQ), self.addr(ACQ), 0, 0, 1));
            }
        }
        if offset >= DBS && (offset - DBS).is_multiple_of(8) && !self.timeout {
            let id = ((offset - DBS) / 8) as u16;
            if self.delay_io && id != 0 {
                self.pending = Some(id);
            } else {
                self.execute(id);
            }
        }
    }
    fn delay_us(&mut self, _: u32) {}
    fn interrupt_enabled(&self) -> bool {
        self.irq
    }
    fn interrupt_generation(&self) -> u64 {
        self.delivered.load(core::sync::atomic::Ordering::Acquire)
    }
    fn wait_completion(&mut self, _: u64) {
        if let Some(id) = self.pending.take() {
            self.execute(id);
        }
    }
}
#[test]
fn content_prp_list_two_queues_and_wrap() {
    let mut controller = Controller::<Host, _>::new(Fake::new(), true, 0x2000).unwrap();
    assert_eq!(controller.queue_count(), 2);
    let data: alloc::vec::Vec<u8> = (0..128 * 1024)
        .map(|i| ((i * 73 + i / 251) & 255) as u8)
        .collect();
    let mut out = vec![0; data.len()];
    for _ in 0..70 {
        controller.write_block(12, &data).unwrap();
        controller.read_block(12, &mut out).unwrap();
        assert_eq!(data, out);
    }
    controller.flush().unwrap();
}
#[test]
fn read_only_admission_and_ranges() {
    let mut c = Controller::<Host, _>::new(Fake::new(), false, 0x2000).unwrap();
    assert!(matches!(
        c.write_block(0, &[0; 512]),
        Err(DevError::Unsupported)
    ));
    assert!(matches!(
        c.read_block(2048, &mut [0; 512]),
        Err(DevError::InvalidParam)
    ));
    c.flush().unwrap();
    assert!(c.read_only());
}
#[test]
fn timeout_poison_blocks_reuse() {
    let mut bus = Fake::new();
    bus.timeout = true;
    assert!(Controller::<Host, _>::new(bus, true, 0x2000).is_err());
}
#[test]
fn prp_offset_boundaries() {
    let mut list = [0; 32];
    assert_eq!(prps(0x1100, 3840, 0x9000, &mut list).unwrap(), (0x1100, 0));
    assert_eq!(
        prps(0x1100, 3841, 0x9000, &mut list).unwrap(),
        (0x1100, 0x2000)
    );
    assert_eq!(
        prps(0x1100, 9000, 0x9000, &mut list).unwrap(),
        (0x1100, 0x9000)
    );
    assert_eq!(&list[..2], &[0x2000, 0x3000]);
    assert!(prps(0x1000, 9000, 0x9001, &mut list).is_err());
}

#[test]
fn interrupts_and_lost_interrupt_polling_keep_one_cq_owner() {
    for interrupt in [true, false] {
        let mut bus = Fake::new();
        bus.irq = interrupt;
        bus.delay_io = true;
        let delivered = bus.delivered.clone();
        let mut controller = Controller::<Host, _>::new(bus, true, 0x2000).unwrap();
        let before = delivered.load(core::sync::atomic::Ordering::Acquire);
        controller.write_block(8, &[0xa5; 8192]).unwrap();
        let mut out = [0; 8192];
        controller.read_block(8, &mut out).unwrap();
        controller.flush().unwrap();
        assert_eq!(out, [0xa5; 8192]);
        assert_eq!(
            delivered.load(core::sync::atomic::Ordering::Acquire) - before,
            if interrupt { 3 } else { 0 }
        );
    }
}

#[test]
fn mdts_scaling_never_truncates_or_widens_a_small_limit() {
    for mdts in 0..=u8::MAX {
        let expected = match mdts {
            1..=4 => 4096usize << mdts,
            _ => 128 * 1024,
        };
        assert_eq!(
            crate::bringup::transfer_limit(mdts),
            expected,
            "MDTS={mdts}"
        );
    }
}

#[test]
fn namespace_format_selection_and_admission() {
    let mut data = [0u8; 4096];
    let blocks = (1u64 << 40) + 7;
    data[..8].copy_from_slice(&blocks.to_le_bytes());
    // Advertise 64 formats; reject interpreting an extended index as index zero.
    data[25] = 63;
    data[130] = 9;
    for format in 0..64usize {
        data[26] = (format as u8 & 15) | ((format as u8 & 48) << 1);
        let offset = 128 + format * 4;
        data[offset + 2] = 12;
        assert_eq!(
            crate::bringup::namespace_geometry(&data).unwrap(),
            (blocks, 4096)
        );
        data[offset + 2] = 0;
    }
    data[26] = 0;
    data[130] = 9;
    data[25] = 0;
    assert_eq!(
        crate::bringup::namespace_geometry(&data).unwrap(),
        (blocks, 512)
    );
    data[26] = 1;
    assert!(crate::bringup::namespace_geometry(&data).is_err());
    data[26] = 0;
    data[25] = 64;
    assert!(crate::bringup::namespace_geometry(&data).is_err());
    data[25] = 0;
    data[128] = 8;
    assert!(crate::bringup::namespace_geometry(&data).is_err());
    data[128] = 0;
    data[29] = 1;
    assert!(crate::bringup::namespace_geometry(&data).is_err());
    data[29] = 0;
    for shift in [0, 8, 13, 63, 255] {
        data[130] = shift;
        assert!(crate::bringup::namespace_geometry(&data).is_err());
    }
    data[130] = 9;
    data[..8].fill(0);
    assert!(crate::bringup::namespace_geometry(&data).is_err());
    assert!(crate::bringup::namespace_geometry(&data[..4095]).is_err());
}

#[test]
fn large_mdts_keeps_bounded_io_working() {
    for mdts in [52, 63, 64, 255] {
        let mut bus = Fake::new();
        bus.mdts = mdts;
        let mut controller = Controller::<Host, _>::new(bus, true, 0x2000).unwrap();
        let input = vec![0x6d; 128 * 1024];
        let mut output = vec![0; input.len()];
        controller.write_block(0, &input).unwrap();
        controller.read_block(0, &mut output).unwrap();
        assert_eq!(input, output);
    }
}

#[test]
fn selected_extended_format_controls_controller_geometry() {
    let mut bus = Fake::new();
    bus.namespace[25] = 17;
    bus.namespace[26] = 0x21; // format 17, not format 1
    bus.namespace[128 + 17 * 4 + 2] = 12;
    let controller = Controller::<Host, _>::new(bus, false, 0x2000).unwrap();
    assert_eq!(controller.block_size(), 4096);
    assert_eq!(controller.num_blocks(), 2048);
}

#[test]
fn small_mdts_splits_transfers_without_losing_content() {
    let mut bus = Fake::new();
    bus.mdts = 1;
    let mut controller = Controller::<Host, _>::new(bus, true, 0x2000).unwrap();
    let input: alloc::vec::Vec<u8> = (0..128 * 1024).map(|i| (i / 512) as u8).collect();
    let mut output = vec![0; input.len()];
    controller.write_block(0, &input).unwrap();
    controller.read_block(0, &mut output).unwrap();
    assert_eq!(input, output);
}
