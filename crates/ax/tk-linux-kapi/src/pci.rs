// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! PCI configuration accessors, device search, and mapping helpers.
//! Upstream reference: `drivers/pci/access.c`, `drivers/pci/search.c`, `include/linux/pci.h`.

#![allow(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{c_int, c_uint, c_ulong, c_void};
use core::sync::atomic::{AtomicU32, Ordering};

pub const PCI_ANY_ID: u32 = !0;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PciDeviceId {
    pub vendor: u32,
    pub device: u32,
    pub subvendor: u32,
    pub subdevice: u32,
    pub class: u32,
    pub class_mask: u32,
    pub driver_data: usize,
    pub override_only: u32,
}

pub struct MockPciDevice {
    pub vendor: u32,
    pub device: u32,
    pub subvendor: u32,
    pub subdevice: u32,
    pub class: u32,
    pub config: [u8; 256],
    pub refcount: AtomicU32,
}

static MOCK_DEVICES: spin::Mutex<Vec<Box<MockPciDevice>>> = spin::Mutex::new(Vec::new());

/// Register a mock PCI device for testing or fallback search.
pub fn register_mock_pci_device(
    vendor: u32,
    device: u32,
    subvendor: u32,
    subdevice: u32,
    class: u32,
) -> *mut c_void {
    let mut dev = Box::new(MockPciDevice {
        vendor,
        device,
        subvendor,
        subdevice,
        class,
        config: [0; 256],
        refcount: AtomicU32::new(1),
    });
    dev.config[0..2].copy_from_slice(&(vendor as u16).to_le_bytes());
    dev.config[2..4].copy_from_slice(&(device as u16).to_le_bytes());
    dev.config[8..12].copy_from_slice(&class.to_le_bytes());
    dev.config[0x2c..0x2e].copy_from_slice(&(subvendor as u16).to_le_bytes());
    dev.config[0x2e..0x30].copy_from_slice(&(subdevice as u16).to_le_bytes());

    let raw = Box::into_raw(dev);
    MOCK_DEVICES.lock().push(unsafe { Box::from_raw(raw) });
    raw.cast()
}

/// Clear mock PCI devices registered for testing.
pub fn clear_mock_pci_devices() {
    MOCK_DEVICES.lock().clear();
}

unsafe fn mock_read_config_dword(dev: *mut c_void, offset: u32, val: *mut u32) -> c_int {
    if dev.is_null() || val.is_null() || offset + 4 > 256 || offset & 3 != 0 {
        return -crate::config::EINVAL;
    }
    let mock = unsafe { &*dev.cast::<MockPciDevice>() };
    let bytes: [u8; 4] = mock.config[offset as usize..offset as usize + 4]
        .try_into()
        .unwrap();
    unsafe { *val = u32::from_le_bytes(bytes) };
    0
}

unsafe fn mock_write_config_dword(dev: *mut c_void, offset: u32, val: u32) -> c_int {
    if dev.is_null() || offset + 4 > 256 || offset & 3 != 0 {
        return -crate::config::EINVAL;
    }
    let mock = unsafe { &mut *dev.cast::<MockPciDevice>() };
    mock.config[offset as usize..offset as usize + 4].copy_from_slice(&val.to_le_bytes());
    0
}

unsafe fn mock_bus_read_config_dword(
    _bus: *mut c_void,
    devfn: c_uint,
    offset: u32,
    val: *mut u32,
) -> c_int {
    let devices = MOCK_DEVICES.lock();
    if let Some(dev) = devices.first() {
        let _ = devfn;
        mock_read_config_dword(dev.as_ref() as *const MockPciDevice as *mut c_void, offset, val)
    } else {
        -crate::config::ENODEV
    }
}

unsafe fn put_mock_pci_dev(dev: *mut c_void) {
    if dev.is_null() {
        return;
    }
    let mock = dev.cast::<MockPciDevice>();
    let _ = (*mock).refcount.fetch_sub(1, Ordering::AcqRel);
}

unsafe fn is_mock_device(dev: *mut c_void) -> bool {
    let devices = MOCK_DEVICES.lock();
    devices.iter().any(|d| d.as_ref() as *const MockPciDevice as *mut c_void == dev)
}

// upstream: drivers/pci/access.c pci_read_config_dword()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_read_config_dword(
    dev: *mut c_void,
    offset: c_int,
    val: *mut u32,
) -> c_int {
    if dev.is_null() || val.is_null() || offset < 0 {
        return -crate::config::EINVAL;
    }
    if is_mock_device(dev) {
        return unsafe { mock_read_config_dword(dev, offset as u32, val) };
    }
    if let Some(bus) = crate::kernel_core::PCI_BUS.get() {
        unsafe { (bus.read_config_dword)(dev, offset as u32, val) }
    } else {
        unsafe { mock_read_config_dword(dev, offset as u32, val) }
    }
}

// upstream: drivers/pci/access.c pci_write_config_dword()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_write_config_dword(
    dev: *mut c_void,
    offset: c_int,
    val: u32,
) -> c_int {
    if dev.is_null() || offset < 0 {
        return -crate::config::EINVAL;
    }
    if is_mock_device(dev) {
        return unsafe { mock_write_config_dword(dev, offset as u32, val) };
    }
    if let Some(bus) = crate::kernel_core::PCI_BUS.get() {
        unsafe { (bus.write_config_dword)(dev, offset as u32, val) }
    } else {
        unsafe { mock_write_config_dword(dev, offset as u32, val) }
    }
}

// upstream: drivers/pci/access.c pci_write_config_word()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_write_config_word(
    dev: *mut c_void,
    offset: c_int,
    val: u16,
) -> c_int {
    if dev.is_null() || offset < 0 || offset & 1 != 0 {
        return -crate::config::EINVAL;
    }
    let aligned = (offset as u32) & !3;
    let shift = ((offset as u32) & 2) * 8;
    let mut dword = 0u32;
    let ret = unsafe { pci_read_config_dword(dev, aligned as c_int, &mut dword) };
    if ret != 0 {
        return ret;
    }
    dword = (dword & !(0xffff << shift)) | (u32::from(val) << shift);
    unsafe { pci_write_config_dword(dev, aligned as c_int, dword) }
}

// upstream: drivers/pci/access.c pci_bus_read_config_dword()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_bus_read_config_dword(
    bus: *mut c_void,
    devfn: c_uint,
    where_: c_int,
    val: *mut u32,
) -> c_int {
    if val.is_null() || where_ < 0 {
        return -crate::config::EINVAL;
    }
    if let Some(pci_bus_ops) = crate::kernel_core::PCI_BUS.get() {
        if !bus.is_null() {
            let bridge = unsafe { (pci_bus_ops.domain_host_bridge)(bus) };
            if !bridge.is_null() {
                let ret = unsafe { (pci_bus_ops.read_config_dword)(bridge, where_ as u32, val) };
                unsafe { (pci_bus_ops.pci_dev_put)(bridge) };
                return ret;
            }
        }
    }
    unsafe { mock_bus_read_config_dword(bus, devfn, where_ as u32, val) }
}

// upstream: drivers/pci/access.c pci_bus_read_config_word()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_bus_read_config_word(
    bus: *mut c_void,
    devfn: c_uint,
    where_: c_int,
    val: *mut u16,
) -> c_int {
    if val.is_null() || where_ < 0 || where_ & 1 != 0 {
        return -crate::config::EINVAL;
    }
    let aligned = (where_ as u32) & !3;
    let shift = ((where_ as u32) & 2) * 8;
    let mut dword = 0u32;
    let ret = unsafe { pci_bus_read_config_dword(bus, devfn, aligned as c_int, &mut dword) };
    if ret != 0 {
        return ret;
    }
    unsafe { *val = (dword >> shift) as u16 };
    0
}

fn pci_match_one_device(id: &PciDeviceId, dev: &MockPciDevice) -> bool {
    (id.vendor == PCI_ANY_ID || id.vendor == dev.vendor)
        && (id.device == PCI_ANY_ID || id.device == dev.device)
        && (id.subvendor == PCI_ANY_ID || id.subvendor == dev.subvendor)
        && (id.subdevice == PCI_ANY_ID || id.subdevice == dev.subdevice)
        && !((id.class ^ dev.class) & id.class_mask != 0)
}

// upstream: drivers/pci/search.c pci_dev_present()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_dev_present(ids: *const PciDeviceId) -> c_int {
    if ids.is_null() {
        return 0;
    }
    let devices = MOCK_DEVICES.lock();
    let mut curr = ids;
    unsafe {
        while (*curr).vendor != 0 || (*curr).subvendor != 0 || (*curr).class_mask != 0 {
            for dev in devices.iter() {
                if pci_match_one_device(&*curr, dev.as_ref()) {
                    return 1;
                }
            }
            curr = curr.add(1);
        }
    }
    0
}

// upstream: drivers/pci/search.c pci_get_class()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_get_class(class: c_uint, from: *mut c_void) -> *mut c_void {
    if !from.is_null() {
        unsafe { pci_dev_put(from) };
    }
    let devices = MOCK_DEVICES.lock();
    let mut found_from = from.is_null();
    for dev in devices.iter() {
        let dev_ptr = dev.as_ref() as *const MockPciDevice as *mut c_void;
        if !found_from {
            if dev_ptr == from {
                found_from = true;
            }
            continue;
        }
        if (dev.class >> 8) == (class >> 8) {
            dev.refcount.fetch_add(1, Ordering::Relaxed);
            return dev_ptr;
        }
    }
    core::ptr::null_mut()
}

// upstream: drivers/pci/pci.c pci_dev_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_dev_put(dev: *mut c_void) {
    if dev.is_null() {
        return;
    }
    if is_mock_device(dev) {
        unsafe { put_mock_pci_dev(dev) };
    } else if let Some(bus) = crate::kernel_core::PCI_BUS.get() {
        unsafe { (bus.pci_dev_put)(dev) };
    } else {
        unsafe { put_mock_pci_dev(dev) };
    }
}

// upstream: drivers/pci/pci-driver.c pci_match_id()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_match_id(
    ids: *const PciDeviceId,
    dev: *mut c_void,
) -> *const PciDeviceId {
    if ids.is_null() || dev.is_null() {
        return core::ptr::null();
    }
    let devices = MOCK_DEVICES.lock();
    let mock = devices
        .iter()
        .find(|d| d.as_ref() as *const MockPciDevice as *mut c_void == dev);
    if let Some(mock) = mock {
        let mut curr = ids;
        unsafe {
            while (*curr).vendor != 0 || (*curr).subvendor != 0 || (*curr).class_mask != 0 {
                if pci_match_one_device(&*curr, mock.as_ref()) {
                    return curr;
                }
                curr = curr.add(1);
            }
        }
        return core::ptr::null();
    }
    drop(devices);

    let mut id_word = 0u32;
    if unsafe { pci_read_config_dword(dev, 0, &mut id_word) } != 0 {
        return core::ptr::null();
    }
    let vendor = id_word & 0xffff;
    let device = (id_word >> 16) & 0xffff;
    let mut sub_word = 0u32;
    let _ = unsafe { pci_read_config_dword(dev, 0x2c, &mut sub_word) };
    let subvendor = sub_word & 0xffff;
    let subdevice = (sub_word >> 16) & 0xffff;
    let mut class_word = 0u32;
    let _ = unsafe { pci_read_config_dword(dev, 8, &mut class_word) };
    let class = class_word >> 8;
    let temp_dev = MockPciDevice {
        vendor,
        device,
        subvendor,
        subdevice,
        class,
        config: [0; 256],
        refcount: AtomicU32::new(1),
    };
    let mut curr = ids;
    unsafe {
        while (*curr).vendor != 0 || (*curr).subvendor != 0 || (*curr).class_mask != 0 {
            if pci_match_one_device(&*curr, &temp_dev) {
                return curr;
            }
            curr = curr.add(1);
        }
    }
    core::ptr::null()
}

// upstream: drivers/pci/pci.c pci_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_free(dev: *mut c_void) {
    unsafe { pci_dev_put(dev) };
}

// upstream: drivers/pci/pci.c pci_iomap_range()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_iomap_range(
    dev: *mut c_void,
    bar: c_int,
    offset: c_ulong,
    maxlen: c_ulong,
) -> *mut c_void {
    if dev.is_null() || bar < 0 {
        return core::ptr::null_mut();
    }
    let start = unsafe { crate::kernel_services::pci_resource_start(dev, bar as u32) };
    let len = unsafe { crate::kernel_services::pci_resource_len(dev, bar as u32) };
    if start == 0 || len <= offset as u64 {
        return core::ptr::null_mut();
    }
    let mut map_len = len - offset as u64;
    if maxlen > 0 && map_len > maxlen as u64 {
        map_len = maxlen as u64;
    }
    unsafe { crate::primitives::ioremap(start + offset as u64, map_len) }
}

// upstream: drivers/pci/pci.c pci_iounmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_iounmap(_dev: *mut c_void, addr: *mut c_void) {
    if !addr.is_null() {
        unsafe { crate::primitives::iounmap(addr) };
    }
}

// upstream: drivers/pci/rom.c pci_map_rom()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_map_rom(_dev: *mut c_void, size: *mut usize) -> *mut c_void {
    // TheKernel does not load or execute option ROMs. intel_rom.c is optional.
    if !size.is_null() {
        unsafe { *size = 0 };
    }
    core::ptr::null_mut()
}

// upstream: drivers/pci/rom.c pci_unmap_rom()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_unmap_rom(_dev: *mut c_void, _rom: *mut c_void) {
    // No option ROM mapping exists.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pci_config_dword_and_word_roundtrip() {
        clear_mock_pci_devices();
        let dev = register_mock_pci_device(0x8086, 0x1234, 0x8086, 0x5678, 0x030000);
        assert!(!dev.is_null());

        let mut read_val = 0u32;
        let ret = unsafe { pci_read_config_dword(dev, 0, &mut read_val) };
        assert_eq!(ret, 0);
        assert_eq!(read_val & 0xffff, 0x8086);
        assert_eq!((read_val >> 16) & 0xffff, 0x1234);

        // Word write and read
        let ret_w = unsafe { pci_write_config_word(dev, 0x50, 0xabcd) };
        assert_eq!(ret_w, 0);

        let mut dword_at_50 = 0u32;
        let ret_r = unsafe { pci_read_config_dword(dev, 0x50, &mut dword_at_50) };
        assert_eq!(ret_r, 0);
        assert_eq!(dword_at_50 & 0xffff, 0xabcd);

        // Word write at high half
        let ret_w2 = unsafe { pci_write_config_word(dev, 0x52, 0x1234) };
        assert_eq!(ret_w2, 0);
        let ret_r2 = unsafe { pci_read_config_dword(dev, 0x50, &mut dword_at_50) };
        assert_eq!(ret_r2, 0);
        assert_eq!(dword_at_50, 0x1234_abcd);

        // Bus read
        let mut bus_word = 0u16;
        let ret_bus = unsafe { pci_bus_read_config_word(core::ptr::null_mut(), 0, 0x50, &mut bus_word) };
        assert_eq!(ret_bus, 0);
        assert_eq!(bus_word, 0xabcd);

        // Bus read dword
        let mut bus_dword = 0u32;
        let ret_bus_dw = unsafe { pci_bus_read_config_dword(core::ptr::null_mut(), 0, 0x50, &mut bus_dword) };
        assert_eq!(ret_bus_dw, 0);
        assert_eq!(bus_dword, 0x1234_abcd);

        unsafe { pci_dev_put(dev) };
    }

    #[test]
    fn test_pci_get_class_and_dev_present() {
        clear_mock_pci_devices();
        let vga = register_mock_pci_device(0x8086, 0x46d1, 0x8086, 0x2211, 0x030000);
        let host = register_mock_pci_device(0x8086, 0x4601, 0x8086, 0x2211, 0x060000);

        // Test pci_get_class
        let found_vga = unsafe { pci_get_class(0x030000, core::ptr::null_mut()) };
        assert_eq!(found_vga, vga);
        let next_vga = unsafe { pci_get_class(0x030000, found_vga) };
        assert!(next_vga.is_null());

        let found_host = unsafe { pci_get_class(0x060000, core::ptr::null_mut()) };
        assert_eq!(found_host, host);
        unsafe { pci_dev_put(found_host) };

        // Test pci_dev_present
        let match_ids = [
            PciDeviceId {
                vendor: 0x8086,
                device: 0x46d1,
                subvendor: PCI_ANY_ID,
                subdevice: PCI_ANY_ID,
                class: 0,
                class_mask: 0,
                driver_data: 0,
                override_only: 0,
            },
            PciDeviceId {
                vendor: 0,
                device: 0,
                subvendor: 0,
                subdevice: 0,
                class: 0,
                class_mask: 0,
                driver_data: 0,
                override_only: 0,
            },
        ];
        assert_eq!(unsafe { pci_dev_present(match_ids.as_ptr()) }, 1);
        assert_eq!(unsafe { pci_match_id(match_ids.as_ptr(), vga) }, match_ids.as_ptr());

        let no_match_ids = [
            PciDeviceId {
                vendor: 0x10de,
                device: 0x1234,
                subvendor: PCI_ANY_ID,
                subdevice: PCI_ANY_ID,
                class: 0,
                class_mask: 0,
                driver_data: 0,
                override_only: 0,
            },
            PciDeviceId {
                vendor: 0,
                device: 0,
                subvendor: 0,
                subdevice: 0,
                class: 0,
                class_mask: 0,
                driver_data: 0,
                override_only: 0,
            },
        ];
        assert_eq!(unsafe { pci_dev_present(no_match_ids.as_ptr()) }, 0);
        assert!(unsafe { pci_match_id(no_match_ids.as_ptr(), vga) }.is_null());

        // Test pci_map_rom returns NULL
        let mut size = 12345usize;
        let rom = unsafe { pci_map_rom(vga, &mut size) };
        assert!(rom.is_null());
        assert_eq!(size, 0);
        unsafe { pci_unmap_rom(vga, rom) };
    }
}
