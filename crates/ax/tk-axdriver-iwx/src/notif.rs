//! Receive notification-ring consumption from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{DeviceFamily, RingError, RxCompletion, RxRing};

pub const RFH_Q0_FRBDCB_WIDX_TRG: u32 = 0x1c80;
pub const HBUS_TARG_WRPTR: u32 = 0x0460;
pub const HBUS_WRPTR_RX_Q0: u32 = 512 << 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationRingError {
    Ring(RingError),
    AllocationFailed,
}

impl From<RingError> for NotificationRingError {
    fn from(error: RingError) -> Self {
        Self::Ring(error)
    }
}

/// Completed RX buffers and the aligned hardware write-pointer return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxNotificationBatch {
    pub completions: Vec<RxCompletion>,
    pub return_register: u32,
    pub return_value: u32,
}

/// Consume RBD completions up to the status write pointer and compute firmware ack.
// upstream: if_iwx.c iwx_notif_intr()
pub fn drain_rx_notifications<R: crate::DmaRegion>(
    ring: &mut RxRing<R>,
    family: DeviceFamily,
) -> Result<RxNotificationBatch, NotificationRingError> {
    let hardware = ring.hardware_index()?;
    let mut completions = Vec::new();
    let mut remaining =
        (hardware + crate::rings::RX_MQ_RING_COUNT - ring.current) % crate::rings::RX_MQ_RING_COUNT;
    completions
        .try_reserve_exact(remaining)
        .map_err(|_| NotificationRingError::AllocationFailed)?;
    while ring.current != hardware {
        completions.push(ring.completion(ring.current)?);
        ring.current = (ring.current + 1) % crate::rings::RX_MQ_RING_COUNT;
        remaining -= 1;
        if remaining == 0 && ring.current != hardware {
            return Err(NotificationRingError::Ring(RingError::InvalidCompletion));
        }
    }
    let previous = if hardware == 0 {
        crate::rings::RX_MQ_RING_COUNT - 1
    } else {
        hardware - 1
    };
    let aligned_pointer = (previous as u32) & !7;
    let (return_register, return_value) = if family >= DeviceFamily::Bz {
        (HBUS_TARG_WRPTR, aligned_pointer | HBUS_WRPTR_RX_Q0)
    } else {
        (RFH_Q0_FRBDCB_WIDX_TRG, aligned_pointer)
    };
    Ok(RxNotificationBatch {
        completions,
        return_register,
        return_value,
    })
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::{DmaError, DmaRegion};

    struct Region {
        address: u64,
        bytes: Vec<u8>,
    }
    impl DmaRegion for Region {
        fn device_address(&self) -> u64 {
            self.address
        }
        fn capacity(&self) -> usize {
            self.bytes.len()
        }
        fn write(&mut self, bytes: &[u8]) -> Result<(), DmaError> {
            self.bytes.copy_from_slice(bytes);
            Ok(())
        }
        fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<(), DmaError> {
            self.bytes
                .get_mut(offset..offset + bytes.len())
                .ok_or(DmaError::RegionTooSmall)?
                .copy_from_slice(bytes);
            Ok(())
        }
        fn read_at(&self, offset: usize, bytes: &mut [u8]) -> Result<(), DmaError> {
            bytes.copy_from_slice(
                self.bytes
                    .get(offset..offset + bytes.len())
                    .ok_or(DmaError::RegionTooSmall)?,
            );
            Ok(())
        }
    }

    #[test]
    fn notification_ring_wraps_and_returns_aligned_widx() {
        let mut used = Region {
            address: 0x2000,
            bytes: vec![
                0;
                crate::rings::RX_MQ_RING_COUNT
                    * crate::rings::RX_COMPLETION_DESCRIPTOR_BYTES
            ],
        };
        for (index, id) in [(510usize, 77u16), (511, 78), (0, 79), (1, 80)] {
            let offset = index * crate::rings::RX_COMPLETION_DESCRIPTOR_BYTES;
            used.write_at(offset + 4, &id.to_le_bytes()).unwrap();
            used.write_at(offset + 6, &[0]).unwrap();
        }
        let ring = RxRing {
            free_descriptors: Region {
                address: 1,
                bytes: vec![0; 512 * 16],
            },
            status: Region {
                address: 2,
                bytes: vec![2, 0],
            },
            used_descriptors: used,
            buffers: Vec::new(),
            current: 510,
        };
        let mut ring = ring;
        let batch = drain_rx_notifications(&mut ring, DeviceFamily::Ax210).unwrap();
        assert_eq!(
            batch
                .completions
                .iter()
                .map(|item| item.buffer_id)
                .collect::<Vec<_>>(),
            [77, 78, 79, 80]
        );
        assert_eq!(batch.return_register, RFH_Q0_FRBDCB_WIDX_TRG);
        assert_eq!(batch.return_value, 0);
        assert_eq!(ring.current, 2);
    }

    #[test]
    fn bz_notifications_use_shared_hbus_pointer_and_wrap_underflow_to_last_slot() {
        let mut ring = RxRing {
            free_descriptors: Region {
                address: 1,
                bytes: vec![0; 512 * 16],
            },
            status: Region {
                address: 2,
                bytes: vec![0, 0],
            },
            used_descriptors: Region {
                address: 3,
                bytes: vec![
                    0;
                    crate::rings::RX_MQ_RING_COUNT
                        * crate::rings::RX_COMPLETION_DESCRIPTOR_BYTES
                ],
            },
            buffers: Vec::new(),
            current: 0,
        };
        let batch = drain_rx_notifications(&mut ring, DeviceFamily::Bz).unwrap();
        assert!(batch.completions.is_empty());
        assert_eq!(batch.return_register, HBUS_TARG_WRPTR);
        assert_eq!(batch.return_value, 504 | HBUS_WRPTR_RX_Q0);
    }
}
