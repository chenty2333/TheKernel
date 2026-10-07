// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod support;
use std::{
    io::Write,
    process::{Command, Stdio},
};

use tk_intel_display::{Error, hdmi_packet::*};
fn packet(kind: u8, version: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![kind, version, payload.len() as u8, 0];
    p.extend(payload);
    p[3] = hdmi_infoframe_checksum(&p);
    p
}
fn fields(frame: Infoframe) -> Vec<u32> {
    match frame {
        Infoframe::Avi(a) => vec![
            u32::from(a.colorspace),
            u32::from(a.scan_mode),
            u32::from(a.colorimetry),
            u32::from(a.picture_aspect),
            u32::from(a.active_aspect),
            u32::from(a.itc),
            u32::from(a.extended_colorimetry),
            u32::from(a.quantization_range),
            u32::from(a.nups),
            u32::from(a.video_code),
            u32::from(a.ycc_quantization_range),
            u32::from(a.content_type),
            u32::from(a.pixel_repeat),
            u32::from(a.top_bar),
            u32::from(a.bottom_bar),
            u32::from(a.left_bar),
            u32::from(a.right_bar),
        ],
        Infoframe::Spd(s) => s
            .vendor
            .into_iter()
            .chain(s.product)
            .chain([s.sdi])
            .map(u32::from)
            .collect(),
        Infoframe::Vendor(v) => vec![
            u32::from(v.length),
            v.oui,
            u32::from(v.vic),
            v.s3d_struct.map(u32::from).unwrap_or(u32::MAX),
            u32::from(v.s3d_ext_data),
        ],
        Infoframe::Drm(d) => {
            let mut out = vec![u32::from(d.eotf), u32::from(d.metadata_type)];
            out.extend(d.display_primaries.into_iter().flatten().map(u32::from));
            out.extend(d.white_point.map(u32::from));
            out.extend(
                [
                    d.max_display_mastering_luminance,
                    d.min_display_mastering_luminance,
                    d.max_cll,
                    d.max_fall,
                ]
                .map(u32::from),
            );
            out
        }
    }
}
#[test]
fn all_truncations_checksum_and_header_errors_are_bounded() {
    for (kind, version, payload) in [
        (0x82, 2, vec![0; 13]),
        (0x83, 1, vec![b'X'; 25]),
        (0x81, 1, vec![3, 12, 0, 0]),
        (0x87, 1, vec![0; 26]),
    ] {
        let p = packet(kind, version, &payload);
        assert!(hdmi_infoframe_unpack(&p).is_ok());
        for n in 0..p.len() {
            assert!(
                hdmi_infoframe_unpack(&p[..n]).is_err(),
                "kind={kind:x} size={n}"
            );
        }
        let mut bad = p.clone();
        bad[3] ^= 1;
        assert_eq!(hdmi_infoframe_unpack(&bad), Err(Error::InvalidBlock));
        let mut bad = p.clone();
        bad[1] ^= 0xff;
        assert_eq!(hdmi_infoframe_unpack(&bad), Err(Error::InvalidHeader));
        let mut bad = p.clone();
        bad[2] = 255;
        assert!(hdmi_infoframe_unpack(&bad).is_err());
    }
    assert_eq!(
        hdmi_infoframe_unpack(&[0, 0, 0, 0]),
        Err(Error::InvalidHeader)
    );
}
#[test]
fn full_width_spd_text_and_embedded_nul_never_scan_past_fields() {
    let mut payload = [b'X'; 25];
    payload[..8].copy_from_slice(b"TheKerne");
    payload[8..24].copy_from_slice(b"Intel display 13");
    payload[24] = 1;
    let Infoframe::Spd(s) = hdmi_infoframe_unpack(&packet(0x83, 1, &payload)).unwrap() else {
        panic!()
    };
    assert_eq!(&s.vendor, b"TheKerne");
    assert_eq!(&s.product, b"Intel display 13");
    assert_eq!(s.sdi, 1);
    payload[2] = 0;
    payload[11] = 0;
    let Infoframe::Spd(s) = hdmi_infoframe_unpack(&packet(0x83, 1, &payload)).unwrap() else {
        panic!()
    };
    assert_eq!(&s.vendor, b"Th\0\0\0\0\0\0");
    assert_eq!(&s.product[..4], b"Int\0");
    assert!(s.product[4..].iter().all(|b| *b == 0));
}
#[test]
fn avi_bars_active_aspect_and_hdr_u16_fields_follow_source() {
    let mut payload = [0; 13];
    payload[0] = 12;
    payload[1] = 0x2b;
    payload[3] = 16;
    payload[5..7].copy_from_slice(&0x1234u16.to_le_bytes());
    payload[7..9].copy_from_slice(&0x5678u16.to_le_bytes());
    let Infoframe::Avi(a) = hdmi_infoframe_unpack(&packet(0x82, 2, &payload)).unwrap() else {
        panic!()
    };
    assert_eq!(a.active_aspect, 11);
    assert_eq!((a.top_bar, a.bottom_bar), (0x1234, 0x5678));
    assert_eq!(a.video_code, 16);
    let mut hdr = [0; 26];
    hdr[0] = 0xff;
    hdr[1] = 0xfe;
    for (n, chunk) in hdr[2..].as_chunks_mut::<2>().0.iter_mut().enumerate() {
        chunk.copy_from_slice(&(n as u16 * 0x101 + 0x1234).to_le_bytes());
    }
    let Infoframe::Drm(d) = hdmi_infoframe_unpack(&packet(0x87, 1, &hdr)).unwrap() else {
        panic!()
    };
    assert_eq!((d.eotf, d.metadata_type), (7, 6));
    assert_eq!(d.display_primaries[0], [0x1234, 0x1335]);
    assert_eq!(d.max_fall, 0x1d3f);
}
#[test]
fn vendor_lengths_formats_and_3d_extension_are_not_guessed() {
    assert!(hdmi_infoframe_unpack(&packet(0x81, 1, &[3, 12, 0, 0])).is_ok());
    let Infoframe::Vendor(v) =
        hdmi_infoframe_unpack(&packet(0x81, 1, &[3, 12, 0, 0x20, 4])).unwrap()
    else {
        panic!()
    };
    assert_eq!(v.vic, 4);
    assert_eq!(v.s3d_struct, None);
    let Infoframe::Vendor(v) =
        hdmi_infoframe_unpack(&packet(0x81, 1, &[3, 12, 0, 0x40, 0x80, 0x30])).unwrap()
    else {
        panic!()
    };
    assert_eq!(v.s3d_struct, Some(8));
    assert_eq!(v.s3d_ext_data, 3);
    for p in [
        &[3, 12, 0, 0x40, 0x80][..],
        &[3, 12, 0, 0x20][..],
        &[3, 12, 0, 0, 0][..],
        &[3, 12, 1, 0][..],
        &[3, 12, 0, 0x60][..],
    ] {
        assert!(hdmi_infoframe_unpack(&packet(0x81, 1, p)).is_err());
    }
}
#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn decoded_hdmi_packet_fields_and_rejections_match_compiled_upstream() {
    let root = support::reference();
    let source = std::fs::read_to_string(root.join("drivers/video/hdmi.c")).unwrap();
    let fns = [
        "static u8 hdmi_infoframe_checksum(",
        "void hdmi_avi_infoframe_init(",
        "int hdmi_spd_infoframe_init(",
        "int hdmi_vendor_infoframe_init(",
        "int hdmi_drm_infoframe_init(",
        "static int hdmi_avi_infoframe_unpack(",
        "static int hdmi_spd_infoframe_unpack(",
        "static int\nhdmi_vendor_any_infoframe_unpack(",
        "int hdmi_drm_infoframe_unpack_only(",
        "static int hdmi_drm_infoframe_unpack(",
    ]
    .into_iter()
    .map(|s| support::function(&source, s))
    .collect::<Vec<_>>()
    .join("\n");
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
typedef uint8_t u8;
#define EINVAL 22
#define min(a,b) ((a)<(b)?(a):(b))
#define HDMI_INFOFRAME_HEADER_SIZE 4
#define HDMI_AVI_INFOFRAME_SIZE 13
#define HDMI_SPD_INFOFRAME_SIZE 25
#define HDMI_VENDOR_INFOFRAME_SIZE 4
#define HDMI_DRM_INFOFRAME_SIZE 26
#define HDMI_INFOFRAME_SIZE(t) (HDMI_INFOFRAME_HEADER_SIZE+HDMI_##t##_INFOFRAME_SIZE)
#define HDMI_INFOFRAME_TYPE_AVI 130
#define HDMI_INFOFRAME_TYPE_SPD 131
#define HDMI_INFOFRAME_TYPE_VENDOR 129
#define HDMI_INFOFRAME_TYPE_DRM 135
#define HDMI_IEEE_OUI 0x000c03
#define HDMI_3D_STRUCTURE_INVALID -1
#define HDMI_3D_STRUCTURE_SIDE_BY_SIDE_HALF 8
struct hdmi_avi_infoframe {u8 type,version,length,colorspace,scan_mode,colorimetry,picture_aspect,active_aspect;bool itc;u8 extended_colorimetry,quantization_range,nups,video_code,ycc_quantization_range,content_type,pixel_repeat;uint16_t top_bar,bottom_bar,left_bar,right_bar;};
struct hdmi_spd_infoframe {u8 type,version,length;char vendor[8],product[16];u8 sdi;};
struct hdmi_vendor_infoframe {u8 type,version,length;unsigned int oui;u8 vic;int s3d_struct;u8 s3d_ext_data;};
union hdmi_vendor_any_infoframe {struct hdmi_vendor_infoframe hdmi;};
struct hdmi_drm_infoframe {u8 type,version,length,eotf,metadata_type;struct {uint16_t x,y;} display_primaries[3],white_point;uint16_t max_display_mastering_luminance,min_display_mastering_luminance,max_cll,max_fall;};
"#;
    let main = r#"
int main(void) {
    unsigned int size,word;
    while(scanf("%u",&size)==1) {
        unsigned char data[64]={0};for(int n=0;n<32;n++) {if(scanf("%u",&word)!=1) return 2;data[n]=word;}
        int ret=-EINVAL;
        if(data[0]==HDMI_INFOFRAME_TYPE_AVI) {
            struct hdmi_avi_infoframe f={0};ret=hdmi_avi_infoframe_unpack(&f,data,size);
            if(!ret) printf("0 %u %u %u %u %u %u %u %u %u %u %u %u %u %u %u %u %u",f.colorspace,f.scan_mode,f.colorimetry,f.picture_aspect,f.active_aspect,f.itc,f.extended_colorimetry,f.quantization_range,f.nups,f.video_code,f.ycc_quantization_range,f.content_type,f.pixel_repeat,f.top_bar,f.bottom_bar,f.left_bar,f.right_bar);
        } else if(data[0]==HDMI_INFOFRAME_TYPE_SPD) {
            struct hdmi_spd_infoframe f={0};ret=hdmi_spd_infoframe_unpack(&f,data,size);
            if(!ret) {printf("0");for(int n=0;n<8;n++) printf(" %u",(u8)f.vendor[n]);for(int n=0;n<16;n++) printf(" %u",(u8)f.product[n]);printf(" %u",f.sdi);}
        } else if(data[0]==HDMI_INFOFRAME_TYPE_VENDOR) {
            union hdmi_vendor_any_infoframe f={0};ret=hdmi_vendor_any_infoframe_unpack(&f,data,size);struct hdmi_vendor_infoframe *v=&f.hdmi;
            if(!ret) printf("0 %u %u %u %u %u",v->length,v->oui,v->vic,(unsigned int)v->s3d_struct,v->s3d_ext_data);
        } else if(data[0]==HDMI_INFOFRAME_TYPE_DRM) {
            struct hdmi_drm_infoframe f={0};ret=hdmi_drm_infoframe_unpack(&f,data,size);
            if(!ret) {printf("0 %u %u",f.eotf,f.metadata_type);for(int n=0;n<3;n++) printf(" %u %u",f.display_primaries[n].x,f.display_primaries[n].y);printf(" %u %u %u %u %u %u",f.white_point.x,f.white_point.y,f.max_display_mastering_luminance,f.min_display_mastering_luminance,f.max_cll,f.max_fall);}
        }
        if(ret) printf("1");puts("");
    }
}
"#;
    let code = [prefix, &fns, main].join("\n");
    let oracle = support::compile(&code, "hdmi-packets");
    let mut packets = Vec::new();
    for seed in 0..64u8 {
        let mut avi = [0; 13];
        for (n, b) in avi.iter_mut().enumerate() {
            *b = seed.wrapping_mul(17).wrapping_add(n as u8 * 13);
        }
        packets.push(packet(0x82, 2, &avi));
        let mut spd = [0; 25];
        for (n, b) in spd.iter_mut().enumerate() {
            *b = seed.wrapping_add(n as u8 * 7);
        }
        if seed % 3 == 0 {
            spd[2] = 0;
            spd[14] = 0;
        }
        packets.push(packet(0x83, 1, &spd));
        let mut hdr = [0; 26];
        for (n, b) in hdr.iter_mut().enumerate() {
            *b = seed.wrapping_mul(5).wrapping_add(n as u8 * 9);
        }
        packets.push(packet(0x87, 1, &hdr));
        for length in 4..=6 {
            for format in 0..4 {
                let mut v = vec![3, 12, 0, format << 5];
                v.resize(length, seed);
                packets.push(packet(0x81, 1, &v));
            }
        }
    }
    let mut cases = Vec::new();
    let mut input = String::new();
    for p in packets {
        for variant in 0..5 {
            let mut data = p.clone();
            let size = match variant {
                0 => data.len(),
                1 => data.len() - 1,
                2 => {
                    data[3] ^= 1;
                    data.len()
                }
                3 => {
                    data[1] ^= 0xff;
                    data.len()
                }
                _ => {
                    data[2] ^= 0x80;
                    data.len()
                }
            };
            let mut padded = [0; 32];
            padded[..data.len()].copy_from_slice(&data);
            input.push_str(&format!("{size}"));
            for byte in padded {
                input.push_str(&format!(" {byte}"));
            }
            input.push('\n');
            cases.push((data, size));
        }
    }
    let mut child = Command::new(&oracle.executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()).unwrap());
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    assert_eq!(output.lines().count(), cases.len());
    for (line, (data, size)) in output.lines().zip(cases) {
        let result = hdmi_infoframe_unpack(&data[..size]);
        let mut expected = vec![u32::from(result.is_err())];
        if let Ok(f) = result {
            expected.extend(fields(f));
        }
        let actual: Vec<u32> = line
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(actual, expected, "data={data:x?} size={size}");
    }
    println!(
        "4800 AVI/SPD/vendor/HDR decode and rejection cases match compiled local HDMI helpers; \
         SPD reads are field-bounded in Rust"
    );
}
