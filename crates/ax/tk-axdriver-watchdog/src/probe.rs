use crate::{Error, Result, ids::Version};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resources {
    pub tco: u16,
    pub smi: Option<u16>,
    pub gcs: Option<u64>,
}
pub fn resources(version: Version, base: u32, control: u32) -> Result<Resources> {
    match version {
        Version::CnlV6 => {
            let port = base & !1;
            if control & 0x100 == 0 || port == 0 || port > 0xffe0 || port & 0x1f != 0 {
                return Err(Error::Invalid);
            }
            Ok(Resources {
                tco: port as u16,
                smi: None,
                gcs: None,
            })
        }
        Version::Ich9V2 => {
            let pm = base & 0xff80;
            let rcba = u64::from(control & 0xffffc000);
            if pm == 0 || control & 1 == 0 || rcba == 0 {
                return Err(Error::Invalid);
            }
            Ok(Resources {
                tco: (pm + 0x60) as u16,
                smi: Some((pm + 0x30) as u16),
                gcs: Some(rcba + 0x3410),
            })
        }
    }
}
pub fn timeout(line: &str) -> Result<Option<u32>> {
    let token = line
        .split_ascii_whitespace()
        .rev()
        .find_map(|t| t.strip_prefix("watchdog.timeout="));
    token
        .map(|t| {
            t.parse::<u32>().map_err(|_| Error::Invalid).and_then(|n| {
                if (3..=614).contains(&n) {
                    Ok(n)
                } else {
                    Err(Error::Invalid)
                }
            })
        })
        .transpose()
}
