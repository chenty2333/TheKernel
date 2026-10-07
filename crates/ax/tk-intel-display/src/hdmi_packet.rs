// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/video/hdmi.c and include/linux/hdmi.h:
// hdmi_infoframe_checksum, hdmi_{avi,spd,drm}_infoframe_unpack,
// hdmi_vendor_any_infoframe_unpack, hdmi_drm_infoframe_unpack_only and
// initialization defaults (selected HDMI display packets, no audio yet).
// Copyright (C) 2012 Avionic Design GmbH.
// Full upstream MIT permission text (including non-infringement disclaimer):
// ../LICENSE-HDMI-MIT. Checked slices and bounded SPD text are safety divergences.
use crate::{Error, bytes, le16};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Avi {
    pub colorspace: u8,
    pub scan_mode: u8,
    pub colorimetry: u8,
    pub picture_aspect: u8,
    pub active_aspect: u8,
    pub itc: bool,
    pub extended_colorimetry: u8,
    pub quantization_range: u8,
    pub nups: u8,
    pub video_code: u8,
    pub ycc_quantization_range: u8,
    pub content_type: u8,
    pub pixel_repeat: u8,
    pub top_bar: u16,
    pub bottom_bar: u16,
    pub left_bar: u16,
    pub right_bar: u16,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Spd {
    pub vendor: [u8; 8],
    pub product: [u8; 16],
    pub sdi: u8,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Vendor {
    pub length: u8,
    pub oui: u32,
    pub vic: u8,
    pub s3d_struct: Option<u8>,
    pub s3d_ext_data: u8,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Drm {
    pub eotf: u8,
    pub metadata_type: u8,
    pub display_primaries: [[u16; 2]; 3],
    pub white_point: [u16; 2],
    pub max_display_mastering_luminance: u16,
    pub min_display_mastering_luminance: u16,
    pub max_cll: u16,
    pub max_fall: u16,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Infoframe {
    Avi(Avi),
    Spd(Spd),
    Vendor(Vendor),
    Drm(Drm),
}
pub fn hdmi_infoframe_checksum(data: &[u8]) -> u8 {
    0u8.wrapping_sub(data.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)))
}
fn payload(data: &[u8], kind: u8, version: u8, len: usize) -> Result<&[u8], Error> {
    let frame = bytes(data, 0, len + 4)?;
    if frame[0] != kind || frame[1] != version || usize::from(frame[2]) != len {
        return Err(Error::InvalidHeader);
    }
    if hdmi_infoframe_checksum(frame) != 0 {
        return Err(Error::InvalidBlock);
    }
    Ok(&frame[4..])
}
pub fn hdmi_avi_infoframe_unpack(data: &[u8]) -> Result<Avi, Error> {
    let p = payload(data, 0x82, 2, 13)?;
    let (top_bar, bottom_bar) = if p[0] & 8 != 0 {
        (le16(p, 5)?, le16(p, 7)?)
    } else {
        (0, 0)
    };
    let (left_bar, right_bar) = if p[0] & 4 != 0 {
        (le16(p, 9)?, le16(p, 11)?)
    } else {
        (0, 0)
    };
    Ok(Avi {
        colorspace: (p[0] >> 5) & 3,
        scan_mode: p[0] & 3,
        colorimetry: (p[1] >> 6) & 3,
        picture_aspect: (p[1] >> 4) & 3,
        // Source overwrites its conditional assignment unconditionally later.
        active_aspect: p[1] & 15,
        itc: p[2] & 0x80 != 0,
        extended_colorimetry: (p[2] >> 4) & 7,
        quantization_range: (p[2] >> 2) & 3,
        nups: p[2] & 3,
        video_code: p[3] & 0x7f,
        ycc_quantization_range: (p[4] >> 6) & 3,
        content_type: (p[4] >> 4) & 3,
        pixel_repeat: p[4] & 15,
        top_bar,
        bottom_bar,
        left_bar,
        right_bar,
    })
}
fn bounded_text<const N: usize>(data: &[u8]) -> [u8; N] {
    let mut out = [0; N];
    // The source initializer uses strlen on fixed-width unpack fields. A valid
    // full-width vendor/product is not necessarily NUL-terminated. Never scan
    // past the field or borrowed packet; retain defined prefix/zero-pad behavior.
    for (dst, src) in out.iter_mut().zip(data.iter()) {
        if *src == 0 {
            break;
        }
        *dst = *src;
    }
    out
}
pub fn hdmi_spd_infoframe_unpack(data: &[u8]) -> Result<Spd, Error> {
    let p = payload(data, 0x83, 1, 25)?;
    Ok(Spd {
        vendor: bounded_text(&p[..8]),
        product: bounded_text(&p[8..24]),
        sdi: p[24],
    })
}
pub fn hdmi_vendor_any_infoframe_unpack(data: &[u8]) -> Result<Vendor, Error> {
    let h = bytes(data, 0, 4)?;
    let length = h[2];
    if ![4, 5, 6].contains(&length) {
        return Err(Error::InvalidHeader);
    }
    let p = payload(data, 0x81, 1, usize::from(length))?;
    if p[..3] != [3, 12, 0] {
        return Err(Error::InvalidBlock);
    }
    let mut v = Vendor {
        length,
        oui: 0x000c03,
        vic: 0,
        s3d_struct: None,
        s3d_ext_data: 0,
    };
    match p[3] >> 5 {
        0 if length == 4 => {}
        1 if length == 5 => v.vic = p[4],
        2 if length == 5 || length == 6 => {
            let s3d = p[4] >> 4;
            v.s3d_struct = Some(s3d);
            if s3d >= 8 {
                if length != 6 {
                    return Err(Error::InvalidBlock);
                }
                v.s3d_ext_data = p[5] >> 4;
            }
        }
        _ => return Err(Error::InvalidBlock),
    }
    Ok(v)
}
pub fn hdmi_drm_infoframe_unpack(data: &[u8]) -> Result<Drm, Error> {
    let p = payload(data, 0x87, 1, 26)?;
    let mut display_primaries = [[0; 2]; 3];
    for (n, v) in display_primaries.iter_mut().enumerate() {
        *v = [le16(p, 2 + n * 4)?, le16(p, 4 + n * 4)?];
    }
    Ok(Drm {
        eotf: p[0] & 7,
        metadata_type: p[1] & 7,
        display_primaries,
        white_point: [le16(p, 14)?, le16(p, 16)?],
        max_display_mastering_luminance: le16(p, 18)?,
        min_display_mastering_luminance: le16(p, 20)?,
        max_cll: le16(p, 22)?,
        max_fall: le16(p, 24)?,
    })
}
pub fn hdmi_infoframe_unpack(data: &[u8]) -> Result<Infoframe, Error> {
    let h = bytes(data, 0, 4)?;
    match h[0] {
        0x82 => hdmi_avi_infoframe_unpack(data).map(Infoframe::Avi),
        0x83 => hdmi_spd_infoframe_unpack(data).map(Infoframe::Spd),
        0x81 => hdmi_vendor_any_infoframe_unpack(data).map(Infoframe::Vendor),
        0x87 => hdmi_drm_infoframe_unpack(data).map(Infoframe::Drm),
        _ => Err(Error::InvalidHeader),
    }
}
