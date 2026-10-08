//! Rootfs-ready firmware request for the single integrated AX211 device.
//!
//! The PCI device is discovered before the root filesystem exists. The
//! callback stages bounded firmware bytes; hardware bring-up consumes them only
//! after the rootfs has been mounted and the callback has completed.

use alloc::vec::Vec;

use spin::Mutex;

use crate::{DeviceConfig, FirmwareError, FirmwareImage};

const FW_MAX_BYTES: usize = 4 * 1024 * 1024;
const PNVM_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Firmware files made available after the rootfs-ready callback.
#[derive(Debug)]
pub struct FirmwareBundle {
    pub image: FirmwareImage,
    /// External PNVM file; `None` means the image embeds PNVM data.
    pub pnvm_file: Option<Vec<u8>>,
}

/// Why rootfs firmware could not be staged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareRequestError {
    FirmwareMissing,
    FirmwareInvalid(FirmwareError),
    PnvmMissing,
}

static STAGED: Mutex<Option<Result<FirmwareBundle, FirmwareRequestError>>> = Mutex::new(None);

/// Register rootfs-ready loading for the AX211 ucode and optional PNVM file.
///
/// Called when an AX211 is discovered, before the rootfs is mounted. The
/// callback runs immediately if rootfs firmware is already available.
pub fn request_on_rootfs_ready() -> bool {
    axdriver_base::firmware::on_rootfs_ready(load_ax211_firmware)
}

/// Take the result staged by the one-shot rootfs-ready callback.
pub fn take_staged() -> Option<Result<FirmwareBundle, FirmwareRequestError>> {
    STAGED.lock().take()
}

fn load_ax211_firmware() {
    let result = load_bundle(axdriver_base::firmware::request, None);
    *STAGED.lock() = Some(result);
}

// upstream: if_iwx.c iwx_read_firmware()
fn load_bundle(
    mut request: impl FnMut(&str, usize) -> Option<Vec<u8>>,
    sku_id: Option<[u32; 3]>,
) -> Result<FirmwareBundle, FirmwareRequestError> {
    let config = DeviceConfig::SoGfAx211.firmware();
    let bytes =
        request(config.firmware, FW_MAX_BYTES).ok_or(FirmwareRequestError::FirmwareMissing)?;
    let image = FirmwareImage::parse(&bytes).map_err(FirmwareRequestError::FirmwareInvalid)?;
    let pnvm_file = if sku_id == Some([0; 3]) {
        // upstream: if_iwx.c iwx_load_pnvm()
        None
    } else {
        match (image.pnvm.is_some(), config.pnvm) {
            (true, _) | (_, None) => None,
            (false, Some(path)) => {
                Some(request(path, PNVM_MAX_BYTES).ok_or(FirmwareRequestError::PnvmMissing)?)
            }
        }
    };
    Ok(FirmwareBundle { image, pnvm_file })
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn minimal_api89_image() -> Vec<u8> {
        let mut bytes = vec![0; 88];
        bytes[4..8].copy_from_slice(&0x0a4c_5749u32.to_le_bytes());
        bytes[72..76].copy_from_slice(&((1u32 << 24) | (2 << 16) | (89 << 8)).to_le_bytes());
        bytes
    }

    #[test]
    fn requests_linux_firmware_paths_with_size_caps() {
        let mut requests = Vec::new();
        let bundle = load_bundle(
            |path, max| {
                requests.push((path.to_owned(), max));
                match path {
                    "/lib/firmware/iwlwifi-so-a0-gf-a0-89.ucode" => Some(minimal_api89_image()),
                    "/lib/firmware/iwlwifi-so-a0-gf-a0.pnvm" => Some(vec![1, 2, 3]),
                    _ => None,
                }
            },
            Some([1, 2, 3]),
        )
        .unwrap();
        assert!(bundle.image.sections.is_empty());
        assert_eq!(bundle.pnvm_file.as_deref(), Some(&[1, 2, 3][..]));
        assert_eq!(
            requests,
            [
                (
                    "/lib/firmware/iwlwifi-so-a0-gf-a0-89.ucode".to_owned(),
                    FW_MAX_BYTES
                ),
                (
                    "/lib/firmware/iwlwifi-so-a0-gf-a0.pnvm".to_owned(),
                    PNVM_MAX_BYTES
                ),
            ]
        );
    }

    #[test]
    fn zero_sku_skips_external_pnvm_request() {
        let mut requested = Vec::new();
        let bundle = load_bundle(
            |path, _| {
                requested.push(path.to_owned());
                (path == "/lib/firmware/iwlwifi-so-a0-gf-a0-89.ucode").then(minimal_api89_image)
            },
            Some([0; 3]),
        )
        .unwrap();
        assert!(bundle.pnvm_file.is_none());
        assert_eq!(requested, ["/lib/firmware/iwlwifi-so-a0-gf-a0-89.ucode"]);
    }

    #[test]
    fn refuses_when_required_firmware_is_missing() {
        assert_eq!(
            load_bundle(|_, _| None, None).unwrap_err(),
            FirmwareRequestError::FirmwareMissing
        );
    }
}
