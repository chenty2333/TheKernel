use alloc::{format, string::String, sync::Arc, task::Wake, vec};
use core::{any::Any, mem::size_of, task::Context, time::Duration};

#[allow(unused_imports)]
use axdriver::prelude::{
    AxInputDevice, BaseDriverOps, DevError, Event, EventType, InputDeviceId, InputDriverOps,
};
use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{DeviceId, NodeFlags, NodeType, VfsResult};
use axpoll::{IoEvents, PollRegistrationError, PollSet, Pollable, RegisterError};
use axsync::Mutex;
use axtask::future::{
    IrqWakerRegisterError, IrqWakerToken, cancel_irq_waker, register_irq_waker, update_irq_waker,
};
use bitmaps::Bitmap;
use linux_raw_sys::{
    general::{__kernel_old_time_t, __kernel_suseconds_t},
    ioctl::{EVIOCGID, EVIOCGRAB, EVIOCGVERSION},
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::{
    file::IoctlContext,
    mm::map_usercopy_error,
    pseudofs::{Device, DeviceOps, DirMapping, SimpleFs},
    time::wall_time,
};
const KEY_CNT: usize = EventType::Key.bits_count();

struct Inner {
    device: AxInputDevice,
    read_ahead: Option<(Duration, Event)>,
    key_state: Bitmap<KEY_CNT>,
}
impl Inner {
    fn has_event(&mut self) -> bool {
        if self.read_ahead.is_none() {
            match self.device.read_event() {
                Ok(event) => {
                    if event.event_type == EventType::Key as u16 {
                        if event.value == 0 {
                            self.key_state.set(event.code as usize, false);
                        } else if event.value == 1 {
                            self.key_state.set(event.code as usize, true);
                        }
                    }
                    self.read_ahead = Some((wall_time(), event));
                }
                Err(DevError::Again) => {}
                Err(err) => {
                    warn!("Failed to read event: {err:?}");
                }
            }
        }
        self.read_ahead.is_some()
    }
}

pub struct EventDev {
    inner: Mutex<Inner>,
    ev_bits: Bitmap<{ EventType::COUNT as usize }>,
    irq: Option<usize>,
    irq_waiters: Arc<PollSet>,
    irq_waker: core::task::Waker,
    irq_registration: spin::Mutex<Option<IrqWakerToken>>,
}

struct InputIrqWake(Arc<PollSet>);

impl Wake for InputIrqWake {
    fn wake(self: Arc<Self>) {
        PollSet::wake(self.0.as_ref());
    }

    fn wake_by_ref(self: &Arc<Self>) {
        PollSet::wake(self.0.as_ref());
    }
}

impl EventDev {
    pub fn new(mut device: AxInputDevice) -> Self {
        let mut ev_bits = Bitmap::new();
        for i in 0..EventType::COUNT {
            let Some(ty) = EventType::from_repr(i) else {
                continue;
            };
            if device
                .get_event_bits(ty, &mut [])
                .is_ok_and(|success| success)
            {
                ev_bits.set(i as usize, true);
            }
        }

        // let mut out = [0u8; 2000];
        // if device.get_event_bits(EventType::Absolute, &mut out).unwrap() {
        //     let mut bits = Vec::new();
        //     for i in 0..EventType::Absolute.bits_count() {
        //         if (out[i / 8] >> (i % 8)) & 1 != 0 {
        //             bits.push(i);
        //         }
        //     }
        //     warn!("{bits:?}");
        // } else {
        //     warn!("failure");
        // }
        let irq = device.irq_num();
        // Input-device construction is the IRQ capability owner. Readiness
        // registration only attaches bounded waiters and never toggles the
        // controller as a hidden side effect.
        if let Some(irq) = irq {
            axhal::irq::set_enable(irq, true);
        }
        let irq_waiters = Arc::new(PollSet::new());
        let irq_waker = core::task::Waker::from(Arc::new(InputIrqWake(Arc::clone(&irq_waiters))));
        Self {
            inner: Mutex::new(Inner {
                device,
                read_ahead: None,
                key_state: Bitmap::new(),
            }),
            ev_bits,
            irq,
            irq_waiters,
            irq_waker,
            irq_registration: spin::Mutex::new(None),
        }
    }

    fn ensure_irq_bridge(&self) -> Result<(), PollRegistrationError> {
        let irq = self.irq.ok_or(PollRegistrationError::InvalidState)?;
        let mut registration = self.irq_registration.lock();
        if let Some(token) = *registration {
            if update_irq_waker(token, &self.irq_waker).is_ok() {
                return Ok(());
            }
            *registration = None;
        }
        let token = register_irq_waker(irq, &self.irq_waker).map_err(|error| match error {
            IrqWakerRegisterError::Waiter(error) => {
                PollRegistrationError::Source { index: 0, error }
            }
            IrqWakerRegisterError::SourceCapacityExhausted
            | IrqWakerRegisterError::HookInstallationInProgress => PollRegistrationError::Source {
                index: 0,
                error: RegisterError::Full,
            },
            IrqWakerRegisterError::HookUnavailable => PollRegistrationError::Source {
                index: 0,
                error: RegisterError::Closed,
            },
        })?;
        *registration = Some(token);
        Ok(())
    }

    fn get_event_bits(
        &self,
        context: &IoctlContext,
        arg: usize,
        size: usize,
        ty: u8,
    ) -> AxResult<usize> {
        let mut bits = vec![0; size];
        if ty == 0 {
            let copied = copy_bytes(self.ev_bits.as_bytes(), &mut bits);
            context
                .user_memory()
                .write_bytes(arg, &bits)
                .map_err(map_usercopy_error)?;
            Ok(copied)
        } else {
            let ty = EventType::from_repr(ty).ok_or(AxError::InvalidInput)?;
            match self.inner.lock().device.get_event_bits(ty, &mut bits) {
                Ok(true) => {}
                Ok(false) => {
                    debug!("No events for {ty:?}");
                }
                Err(err) => {
                    warn!("Failed to get event bits: {err:?}");
                }
            }
            context
                .user_memory()
                .write_bytes(arg, &bits)
                .map_err(map_usercopy_error)?;
            Ok(bits.len().min(ty.bits_count().div_ceil(8)))
        }
    }
}

fn copy_bytes(src: &[u8], dst: &mut [u8]) -> usize {
    let len = src.len().min(dst.len());
    dst[..len].copy_from_slice(&src[..len]);
    len
}

fn zero_bits_len(size: usize, bits: usize) -> usize {
    bits.div_ceil(8).min(size)
}

fn return_str(context: &IoctlContext, arg: usize, size: usize, s: &str) -> AxResult<usize> {
    let mut bytes = vec![0; size];
    let copied = copy_bytes(s.as_bytes(), &mut bytes);
    context
        .user_memory()
        .write_bytes(arg, &bytes)
        .map_err(map_usercopy_error)?;
    Ok(copied)
}
fn return_zero_bits(
    context: &IoctlContext,
    arg: usize,
    size: usize,
    bits: usize,
) -> AxResult<usize> {
    let len = zero_bits_len(size, bits);
    let bytes = vec![0; len];
    context
        .user_memory()
        .write_bytes(arg, &bytes)
        .map_err(map_usercopy_error)?;
    Ok(len)
}

#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable)]
pub struct KernelTimeval {
    pub tv_sec: __kernel_old_time_t,
    pub tv_usec: __kernel_suseconds_t,
}

#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable)]
struct InputEvent {
    time: KernelTimeval,
    event_type: u16,
    code: u16,
    value: i32,
}

impl DeviceOps for EventDev {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if buf.len() < size_of::<InputEvent>() {
            return Err(AxError::InvalidInput);
        }
        let mut read = 0;
        let mut inner = self.inner.lock();
        for out in buf.chunks_exact_mut(size_of::<InputEvent>()) {
            if !inner.has_event() {
                break;
            }
            let Some((time, event)) = inner.read_ahead.take() else {
                break;
            };
            let input_event = InputEvent {
                time: KernelTimeval {
                    tv_sec: time.as_secs() as _,
                    tv_usec: time.subsec_micros() as _,
                },
                event_type: event.event_type,
                code: event.code,
                value: event.value as _,
            };
            out.copy_from_slice(input_event.as_bytes());
            read += out.len();
        }
        if read == 0 {
            Err(AxError::WouldBlock)
        } else {
            Ok(read)
        }
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
            | NodeFlags::STREAM
            | NodeFlags::NO_POSITIONED_READ
            | NodeFlags::NO_POSITIONED_WRITE
            | NodeFlags::NO_SEEK
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_pollable(&self) -> Option<&dyn Pollable> {
        Some(self)
    }

    fn ioctl(&self, context: &IoctlContext, arg_cmd: u32, arg: usize) -> VfsResult<usize> {
        let cmd = arg_cmd;
        match cmd {
            EVIOCGVERSION => {
                context
                    .user_memory()
                    .write_bytes(arg, &0x10001u32.to_ne_bytes())
                    .map_err(map_usercopy_error)?;
                Ok(0)
            }
            EVIOCGID => {
                let id = self.inner.lock().device.device_id();
                let mut bytes = [0u8; size_of::<InputDeviceId>()];
                bytes[0..2].copy_from_slice(&id.bus_type.to_ne_bytes());
                bytes[2..4].copy_from_slice(&id.vendor.to_ne_bytes());
                bytes[4..6].copy_from_slice(&id.product.to_ne_bytes());
                bytes[6..8].copy_from_slice(&id.version.to_ne_bytes());
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(map_usercopy_error)?;
                Ok(0)
            }
            // Exclusive grabs require ownership per open file description. DeviceOps is
            // shared by every opener, so claiming success here would not provide EVIOCGRAB.
            EVIOCGRAB => Err(LinuxError::EOPNOTSUPP.into()),
            other => {
                // variable-length command
                let mut tmp = other;
                let nr = (tmp & 0xff) as u8;
                tmp >>= 8;
                let ty = (tmp & 0xff) as u8;
                tmp >>= 8;
                let size = (tmp & 0x3fff) as usize;
                tmp >>= 14;
                let dir = tmp & 0x3;

                if ty != b'E' {
                    warn!("unknown ioctl for evdev: {cmd} {arg}");
                    return Err(AxError::InvalidInput);
                }

                match dir {
                    // IOC_WRITE
                    1 => return Err(AxError::InvalidInput),
                    // IOC_READ
                    2 => {
                        #[allow(clippy::single_match)]
                        match nr {
                            // EVIOCGNAME
                            0x06 => {
                                let name = {
                                    let inner = self.inner.lock();
                                    String::from(inner.device.device_name())
                                };
                                return return_str(context, arg, size, &name);
                            }
                            // EVIOCGPHYS
                            0x07 => {
                                let physical_location = {
                                    let inner = self.inner.lock();
                                    String::from(inner.device.physical_location())
                                };
                                return return_str(context, arg, size, &physical_location);
                            }
                            // EVIOCGUNIQ
                            0x08 => {
                                let unique_id = {
                                    let inner = self.inner.lock();
                                    String::from(inner.device.unique_id())
                                };
                                return return_str(context, arg, size, &unique_id);
                            }
                            // EVIOCGPROP
                            0x09 => {
                                // For some reasons virtio does not provide prop
                                // bits for now. The command encodes the output
                                // length in bytes, so clear exactly that many
                                // bytes without exposing any padding.
                                return return_zero_bits(
                                    context,
                                    arg,
                                    size,
                                    size.saturating_mul(8),
                                );
                            }
                            // EVIOCGKEY
                            0x18 => {
                                let mut bits = vec![0; size];
                                let copied =
                                    copy_bytes(self.inner.lock().key_state.as_bytes(), &mut bits);
                                context
                                    .user_memory()
                                    .write_bytes(arg, &bits)
                                    .map_err(map_usercopy_error)?;
                                return Ok(copied);
                            }
                            // EVIOCGLED
                            0x19 => {
                                return return_zero_bits(
                                    context,
                                    arg,
                                    size,
                                    EventType::Led.bits_count(),
                                );
                            }
                            // EVIOCGSND
                            0x1a => {
                                return return_zero_bits(
                                    context,
                                    arg,
                                    size,
                                    EventType::Sound.bits_count(),
                                );
                            }
                            // EVIOCGSW
                            0x1b => {
                                return return_zero_bits(
                                    context,
                                    arg,
                                    size,
                                    EventType::Switch.bits_count(),
                                );
                            }
                            _ => {}
                        }
                        if nr & !EventType::MAX == EventType::COUNT {
                            return self.get_event_bits(context, arg, size, nr & EventType::MAX);
                        }
                        const ABS_CNT: u8 = 0x40;
                        if nr & !(ABS_CNT - 1) == ABS_CNT {
                            return Err(LinuxError::EOPNOTSUPP.into());
                        }
                        return Err(AxError::InvalidInput);
                    }
                    _ => {}
                }

                Err(AxError::InvalidInput)
            }
        }
    }
}

impl Pollable for EventDev {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::READABLE, self.inner.lock().has_event());
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, PollRegistrationError> {
        if !events.contains(IoEvents::READABLE) {
            return axpoll::PollRegistration::empty();
        }
        let registration = axpoll::PollRegistration::single(&self.irq_waiters, context.waker())?;
        self.ensure_irq_bridge()?;
        Ok(registration)
    }
}

impl Drop for EventDev {
    fn drop(&mut self) {
        if let Some(token) = self.irq_registration.get_mut().take() {
            cancel_irq_waker(token);
        }
        if let Some(irq) = self.irq {
            axhal::irq::set_enable(irq, false);
        }
    }
}

pub fn input_devices(fs: Arc<SimpleFs>) -> DirMapping {
    let mut inputs = DirMapping::new();
    let mut input_id = 0;
    let input_devices = axinput::take_inputs();
    let mut keys = [0; 0x300usize.div_ceil(8)];
    for (i, mut device) in input_devices.into_iter().enumerate() {
        assert!(device.get_event_bits(EventType::Key, &mut keys).unwrap());

        let dev = Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(13, (i + 1) as _),
            Arc::new(EventDev::new(device)),
        );

        const BTN_MOUSE: usize = 0x110;
        if keys[BTN_MOUSE / 8] & (1 << (BTN_MOUSE % 8)) != 0 {
            // Mouse
            inputs.add("mice", dev);
        } else {
            inputs.add(format!("event{input_id}"), dev);
            input_id += 1;
        }
    }
    inputs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_bitmap_uses_the_encoded_byte_length() {
        for size in [0, 1, 2, 7, 8, 9, 0x3fff] {
            assert_eq!(zero_bits_len(size, size.saturating_mul(8)), size);
        }
    }

    #[test]
    fn variable_bitmaps_are_clamped_without_padding() {
        assert_eq!(zero_bits_len(1, 1), 1);
        assert_eq!(zero_bits_len(2, 9), 2);
        assert_eq!(zero_bits_len(8, 9), 2);
    }
}
