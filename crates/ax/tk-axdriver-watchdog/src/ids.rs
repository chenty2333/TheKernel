#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    Ich9V2,
    CnlV6,
}
impl Version {
    pub fn number(self) -> u32 {
        match self {
            Self::Ich9V2 => 2,
            Self::CnlV6 => 6,
        }
    }
}
pub fn identify(vendor: u16, device: u16) -> Option<Version> {
    match (vendor, device) {
        (0x8086, 0x2918) => Some(Version::Ich9V2),
        (0x8086, 0x54a3) => Some(Version::CnlV6),
        _ => None,
    }
}
