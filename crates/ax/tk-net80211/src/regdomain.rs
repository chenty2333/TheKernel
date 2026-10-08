//! Regulatory-domain names and country mapping from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_regdomain.c` rev 1.10 and
//! `ieee80211_regdomain.h` rev 1.9 (ISC). Copyright (c) 2004, 2005
//! Reyk Floeter <reyk@openbsd.org>.

pub const DMN_DEFAULT: u32 = 0x00;
pub const DMN_DEBUG: u32 = 0xf100_0000;
pub const CHANNELS_5GHZ_MIN: u16 = 5005;
pub const CHANNELS_5GHZ_MAX: u16 = 6100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegdomainName {
    pub domain: u32,
    pub name: &'static str,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegdomainMap {
    pub domain: u16,
    pub map5: u32,
    pub map2: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CountryName {
    pub code: u16,
    pub name: &'static str,
    pub domain: u32,
}

pub const REGDOMAIN_NAMES: &[RegdomainName] = &[
    RegdomainName {
        domain: 0xf0000001,
        name: "APL1",
    },
    RegdomainName {
        domain: 0x00000054,
        name: "APL1A",
    },
    RegdomainName {
        domain: 0x00000055,
        name: "APL1_ETSIC",
    },
    RegdomainName {
        domain: 0x00000053,
        name: "APL1_FCCA",
    },
    RegdomainName {
        domain: 0x00000052,
        name: "APL1W",
    },
    RegdomainName {
        domain: 0xf0000002,
        name: "APL2",
    },
    RegdomainName {
        domain: 0x00000046,
        name: "APL2C",
    },
    RegdomainName {
        domain: 0x00000049,
        name: "APL2D",
    },
    RegdomainName {
        domain: 0x00000056,
        name: "APL2_ETSIC",
    },
    RegdomainName {
        domain: 0x00000045,
        name: "APL2W",
    },
    RegdomainName {
        domain: 0xf0000004,
        name: "APL3",
    },
    RegdomainName {
        domain: 0x00000047,
        name: "APL3W",
    },
    RegdomainName {
        domain: 0xf0000008,
        name: "APL4",
    },
    RegdomainName {
        domain: 0x00000042,
        name: "APL4W",
    },
    RegdomainName {
        domain: 0xf0000010,
        name: "APL5",
    },
    RegdomainName {
        domain: 0x00000058,
        name: "APL5W",
    },
    RegdomainName {
        domain: 0xf0040000,
        name: "APLD",
    },
    RegdomainName {
        domain: 0x00000044,
        name: "APL",
    },
    RegdomainName {
        domain: 0xf1000000,
        name: "DEBUG",
    },
    RegdomainName {
        domain: 0xf0000020,
        name: "ETSI1",
    },
    RegdomainName {
        domain: 0x00000037,
        name: "ETSI1W",
    },
    RegdomainName {
        domain: 0xf0000040,
        name: "ETSI2",
    },
    RegdomainName {
        domain: 0x00000035,
        name: "ETSI2W",
    },
    RegdomainName {
        domain: 0xf0000080,
        name: "ETSI3",
    },
    RegdomainName {
        domain: 0x00000032,
        name: "ETSI3A",
    },
    RegdomainName {
        domain: 0xf0000100,
        name: "ETSI4",
    },
    RegdomainName {
        domain: 0x00000038,
        name: "ETSI4C",
    },
    RegdomainName {
        domain: 0x00000030,
        name: "ETSI4W",
    },
    RegdomainName {
        domain: 0xf0000200,
        name: "ETSI5",
    },
    RegdomainName {
        domain: 0x00000039,
        name: "ETSI5W",
    },
    RegdomainName {
        domain: 0xf0000400,
        name: "ETSI6",
    },
    RegdomainName {
        domain: 0x00000034,
        name: "ETSI6W",
    },
    RegdomainName {
        domain: 0xf0000800,
        name: "ETSIA",
    },
    RegdomainName {
        domain: 0xf0001000,
        name: "ETSIB",
    },
    RegdomainName {
        domain: 0xf0002000,
        name: "ETSIC",
    },
    RegdomainName {
        domain: 0x00000033,
        name: "ETSI",
    },
    RegdomainName {
        domain: 0x00000068,
        name: "EU1W",
    },
    RegdomainName {
        domain: 0xf0004000,
        name: "FCC1",
    },
    RegdomainName {
        domain: 0x00000010,
        name: "FCC1A",
    },
    RegdomainName {
        domain: 0x00000011,
        name: "FCC1W",
    },
    RegdomainName {
        domain: 0xf0008000,
        name: "FCC2",
    },
    RegdomainName {
        domain: 0x00000022,
        name: "FCC2C",
    },
    RegdomainName {
        domain: 0x00000020,
        name: "FCC2A",
    },
    RegdomainName {
        domain: 0x00000021,
        name: "FCC2W",
    },
    RegdomainName {
        domain: 0xf0010000,
        name: "FCC3",
    },
    RegdomainName {
        domain: 0x0000003a,
        name: "FCC3A",
    },
    RegdomainName {
        domain: 0xf0020000,
        name: "FCCA",
    },
    RegdomainName {
        domain: 0x00000031,
        name: "FRANCE",
    },
    RegdomainName {
        domain: 0xf0080000,
        name: "MKK1",
    },
    RegdomainName {
        domain: 0x00000048,
        name: "MKK1_FCCA",
    },
    RegdomainName {
        domain: 0x00000040,
        name: "MKK1A",
    },
    RegdomainName {
        domain: 0x0000004a,
        name: "MKK1A1",
    },
    RegdomainName {
        domain: 0x0000004b,
        name: "MKK1A2",
    },
    RegdomainName {
        domain: 0x00000041,
        name: "MKK1B",
    },
    RegdomainName {
        domain: 0xf0100000,
        name: "MKK2",
    },
    RegdomainName {
        domain: 0x00000043,
        name: "MKK2A",
    },
    RegdomainName {
        domain: 0xf0200000,
        name: "MKKA",
    },
    RegdomainName {
        domain: 0x00000000,
        name: "NONE",
    },
    RegdomainName {
        domain: 0xf0400000,
        name: "NONE",
    },
    RegdomainName {
        domain: 0x00000007,
        name: "ETSIB",
    },
    RegdomainName {
        domain: 0x00000008,
        name: "ETSIC",
    },
    RegdomainName {
        domain: 0x00000066,
        name: "WOR01W",
    },
    RegdomainName {
        domain: 0x00000067,
        name: "WOR02W",
    },
    RegdomainName {
        domain: 0x00000060,
        name: "WOR0W",
    },
    RegdomainName {
        domain: 0x00000061,
        name: "WOR1W",
    },
    RegdomainName {
        domain: 0x00000062,
        name: "WOR2W",
    },
    RegdomainName {
        domain: 0x00000063,
        name: "WOR3W",
    },
    RegdomainName {
        domain: 0x00000064,
        name: "WOR4W",
    },
    RegdomainName {
        domain: 0x00000065,
        name: "WOR5_ETSIC",
    },
    RegdomainName {
        domain: 0x00000069,
        name: "WOR9W",
    },
    RegdomainName {
        domain: 0x0000006a,
        name: "WORAW",
    },
    RegdomainName {
        domain: 0x00000003,
        name: "WORLD",
    },
    RegdomainName {
        domain: 0xf0800000,
        name: "WORLD",
    },
];

pub const REGDOMAIN_MAP: &[RegdomainMap] = &[
    RegdomainMap {
        domain: 0x0000,
        map5: 0xf1000000,
        map2: 0xf1000000,
    },
    RegdomainMap {
        domain: 0x0003,
        map5: 0xf0400000,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0007,
        map5: 0xf0400000,
        map2: 0xf0001000,
    },
    RegdomainMap {
        domain: 0x0008,
        map5: 0xf0400000,
        map2: 0xf0002000,
    },
    RegdomainMap {
        domain: 0x0010,
        map5: 0xf0004000,
        map2: 0xf0020000,
    },
    RegdomainMap {
        domain: 0x0011,
        map5: 0xf0004000,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0020,
        map5: 0xf0008000,
        map2: 0xf0020000,
    },
    RegdomainMap {
        domain: 0x0021,
        map5: 0xf0008000,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0022,
        map5: 0xf0008000,
        map2: 0xf0002000,
    },
    RegdomainMap {
        domain: 0x0031,
        map5: 0xf0000080,
        map2: 0xf0000080,
    },
    RegdomainMap {
        domain: 0x003a,
        map5: 0xf0010000,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0037,
        map5: 0xf0000020,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0032,
        map5: 0xf0000080,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0035,
        map5: 0xf0000040,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0036,
        map5: 0xf0000080,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0030,
        map5: 0xf0000100,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0038,
        map5: 0xf0000100,
        map2: 0xf0002000,
    },
    RegdomainMap {
        domain: 0x0039,
        map5: 0xf0000200,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0034,
        map5: 0xf0000400,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0033,
        map5: 0xf0000020,
        map2: 0xf0000020,
    },
    RegdomainMap {
        domain: 0x0040,
        map5: 0xf0080000,
        map2: 0xf0200000,
    },
    RegdomainMap {
        domain: 0x0041,
        map5: 0xf0080000,
        map2: 0xf0200000,
    },
    RegdomainMap {
        domain: 0x0042,
        map5: 0xf0000008,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0043,
        map5: 0xf0100000,
        map2: 0xf0200000,
    },
    RegdomainMap {
        domain: 0x0044,
        map5: 0xf0000001,
        map2: 0xf0400000,
    },
    RegdomainMap {
        domain: 0x0045,
        map5: 0xf0000002,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0046,
        map5: 0xf0000002,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0047,
        map5: 0xf0000004,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0048,
        map5: 0xf0080000,
        map2: 0xf0020000,
    },
    RegdomainMap {
        domain: 0x0049,
        map5: 0xf0000002,
        map2: 0xf0040000,
    },
    RegdomainMap {
        domain: 0x004a,
        map5: 0xf0080000,
        map2: 0xf0200000,
    },
    RegdomainMap {
        domain: 0x004b,
        map5: 0xf0080000,
        map2: 0xf0200000,
    },
    RegdomainMap {
        domain: 0x0052,
        map5: 0xf0000001,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0053,
        map5: 0xf0000001,
        map2: 0xf0020000,
    },
    RegdomainMap {
        domain: 0x0054,
        map5: 0xf0000001,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0055,
        map5: 0xf0000001,
        map2: 0xf0002000,
    },
    RegdomainMap {
        domain: 0x0056,
        map5: 0xf0000002,
        map2: 0xf0002000,
    },
    RegdomainMap {
        domain: 0x0058,
        map5: 0xf0000010,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0060,
        map5: 0xf0800000,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0061,
        map5: 0xf0800000,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0062,
        map5: 0xf0800000,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0063,
        map5: 0xf0800000,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0064,
        map5: 0xf0800000,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0065,
        map5: 0xf0800000,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0066,
        map5: 0xf0800000,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0067,
        map5: 0xf0800000,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0068,
        map5: 0xf0000020,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x0069,
        map5: 0xf0800000,
        map2: 0xf0800000,
    },
    RegdomainMap {
        domain: 0x006a,
        map5: 0xf0800000,
        map2: 0xf0800000,
    },
];

pub const COUNTRY_NAMES: &[CountryName] = &[
    CountryName {
        code: 0,
        name: "00",
        domain: 0x00000000,
    },
    CountryName {
        code: 784,
        name: "ae",
        domain: 0x00000003,
    },
    CountryName {
        code: 8,
        name: "al",
        domain: 0x00000003,
    },
    CountryName {
        code: 51,
        name: "am",
        domain: 0x00000030,
    },
    CountryName {
        code: 32,
        name: "ar",
        domain: 0x00000047,
    },
    CountryName {
        code: 40,
        name: "at",
        domain: 0x00000039,
    },
    CountryName {
        code: 36,
        name: "au",
        domain: 0x00000021,
    },
    CountryName {
        code: 31,
        name: "az",
        domain: 0x00000030,
    },
    CountryName {
        code: 56,
        name: "be",
        domain: 0x00000030,
    },
    CountryName {
        code: 100,
        name: "bg",
        domain: 0x00000034,
    },
    CountryName {
        code: 48,
        name: "bh",
        domain: 0x00000003,
    },
    CountryName {
        code: 96,
        name: "bn",
        domain: 0x00000052,
    },
    CountryName {
        code: 68,
        name: "bo",
        domain: 0x00000055,
    },
    CountryName {
        code: 76,
        name: "br",
        domain: 0x00000008,
    },
    CountryName {
        code: 112,
        name: "by",
        domain: 0x00000003,
    },
    CountryName {
        code: 84,
        name: "bz",
        domain: 0x00000008,
    },
    CountryName {
        code: 124,
        name: "ca",
        domain: 0x00000020,
    },
    CountryName {
        code: 756,
        name: "ch",
        domain: 0x00000035,
    },
    CountryName {
        code: 152,
        name: "cl",
        domain: 0x00000058,
    },
    CountryName {
        code: 156,
        name: "cn",
        domain: 0x00000052,
    },
    CountryName {
        code: 170,
        name: "co",
        domain: 0x00000010,
    },
    CountryName {
        code: 188,
        name: "cr",
        domain: 0x00000003,
    },
    CountryName {
        code: 196,
        name: "cy",
        domain: 0x00000037,
    },
    CountryName {
        code: 203,
        name: "cz",
        domain: 0x00000036,
    },
    CountryName {
        code: 276,
        name: "de",
        domain: 0x00000037,
    },
    CountryName {
        code: 208,
        name: "dk",
        domain: 0x00000037,
    },
    CountryName {
        code: 214,
        name: "do",
        domain: 0x00000010,
    },
    CountryName {
        code: 12,
        name: "dz",
        domain: 0x00000003,
    },
    CountryName {
        code: 218,
        name: "ec",
        domain: 0x00000003,
    },
    CountryName {
        code: 233,
        name: "ee",
        domain: 0x00000037,
    },
    CountryName {
        code: 818,
        name: "eg",
        domain: 0x00000003,
    },
    CountryName {
        code: 724,
        name: "es",
        domain: 0x00000037,
    },
    CountryName {
        code: 255,
        name: "f2",
        domain: 0x00000036,
    },
    CountryName {
        code: 246,
        name: "fi",
        domain: 0x00000037,
    },
    CountryName {
        code: 234,
        name: "fo",
        domain: 0x00000003,
    },
    CountryName {
        code: 250,
        name: "fr",
        domain: 0x00000036,
    },
    CountryName {
        code: 268,
        name: "ge",
        domain: 0x00000030,
    },
    CountryName {
        code: 300,
        name: "gr",
        domain: 0x00000003,
    },
    CountryName {
        code: 320,
        name: "gt",
        domain: 0x00000010,
    },
    CountryName {
        code: 344,
        name: "hk",
        domain: 0x00000021,
    },
    CountryName {
        code: 340,
        name: "hn",
        domain: 0x00000003,
    },
    CountryName {
        code: 191,
        name: "hr",
        domain: 0x00000036,
    },
    CountryName {
        code: 348,
        name: "hu",
        domain: 0x00000035,
    },
    CountryName {
        code: 360,
        name: "id",
        domain: 0x00000003,
    },
    CountryName {
        code: 372,
        name: "ie",
        domain: 0x00000037,
    },
    CountryName {
        code: 376,
        name: "il",
        domain: 0x00000003,
    },
    CountryName {
        code: 356,
        name: "in",
        domain: 0x00000003,
    },
    CountryName {
        code: 368,
        name: "iq",
        domain: 0x00000003,
    },
    CountryName {
        code: 364,
        name: "ir",
        domain: 0x00000052,
    },
    CountryName {
        code: 352,
        name: "is",
        domain: 0x00000037,
    },
    CountryName {
        code: 380,
        name: "it",
        domain: 0x00000037,
    },
    CountryName {
        code: 393,
        name: "j1",
        domain: 0x00000041,
    },
    CountryName {
        code: 394,
        name: "j2",
        domain: 0x00000048,
    },
    CountryName {
        code: 395,
        name: "j3",
        domain: 0x00000043,
    },
    CountryName {
        code: 396,
        name: "j4",
        domain: 0x0000004a,
    },
    CountryName {
        code: 397,
        name: "j5",
        domain: 0x0000004b,
    },
    CountryName {
        code: 388,
        name: "jm",
        domain: 0x00000003,
    },
    CountryName {
        code: 400,
        name: "jo",
        domain: 0x00000003,
    },
    CountryName {
        code: 392,
        name: "jp",
        domain: 0x00000040,
    },
    CountryName {
        code: 411,
        name: "k2",
        domain: 0x00000049,
    },
    CountryName {
        code: 404,
        name: "ke",
        domain: 0x00000003,
    },
    CountryName {
        code: 408,
        name: "kp",
        domain: 0x00000045,
    },
    CountryName {
        code: 410,
        name: "kr",
        domain: 0x00000045,
    },
    CountryName {
        code: 414,
        name: "kw",
        domain: 0x00000003,
    },
    CountryName {
        code: 398,
        name: "kz",
        domain: 0x00000003,
    },
    CountryName {
        code: 422,
        name: "lb",
        domain: 0x00000003,
    },
    CountryName {
        code: 438,
        name: "li",
        domain: 0x00000035,
    },
    CountryName {
        code: 728,
        name: "lk",
        domain: 0x00000003,
    },
    CountryName {
        code: 440,
        name: "lt",
        domain: 0x00000037,
    },
    CountryName {
        code: 442,
        name: "lu",
        domain: 0x00000037,
    },
    CountryName {
        code: 428,
        name: "lv",
        domain: 0x00000003,
    },
    CountryName {
        code: 434,
        name: "ly",
        domain: 0x00000003,
    },
    CountryName {
        code: 504,
        name: "ma",
        domain: 0x00000003,
    },
    CountryName {
        code: 492,
        name: "mc",
        domain: 0x00000030,
    },
    CountryName {
        code: 807,
        name: "mk",
        domain: 0x00000003,
    },
    CountryName {
        code: 446,
        name: "mo",
        domain: 0x00000021,
    },
    CountryName {
        code: 484,
        name: "mx",
        domain: 0x00000010,
    },
    CountryName {
        code: 458,
        name: "my",
        domain: 0x00000003,
    },
    CountryName {
        code: 558,
        name: "ni",
        domain: 0x00000003,
    },
    CountryName {
        code: 528,
        name: "nl",
        domain: 0x00000037,
    },
    CountryName {
        code: 578,
        name: "no",
        domain: 0x00000037,
    },
    CountryName {
        code: 554,
        name: "nz",
        domain: 0x00000022,
    },
    CountryName {
        code: 512,
        name: "om",
        domain: 0x00000003,
    },
    CountryName {
        code: 591,
        name: "pa",
        domain: 0x00000010,
    },
    CountryName {
        code: 604,
        name: "pe",
        domain: 0x00000003,
    },
    CountryName {
        code: 608,
        name: "ph",
        domain: 0x00000011,
    },
    CountryName {
        code: 586,
        name: "pk",
        domain: 0x00000003,
    },
    CountryName {
        code: 616,
        name: "pl",
        domain: 0x00000037,
    },
    CountryName {
        code: 630,
        name: "pr",
        domain: 0x00000010,
    },
    CountryName {
        code: 620,
        name: "pt",
        domain: 0x00000037,
    },
    CountryName {
        code: 600,
        name: "py",
        domain: 0x00000003,
    },
    CountryName {
        code: 634,
        name: "qa",
        domain: 0x00000003,
    },
    CountryName {
        code: 642,
        name: "ro",
        domain: 0x00000003,
    },
    CountryName {
        code: 643,
        name: "ru",
        domain: 0x00000003,
    },
    CountryName {
        code: 682,
        name: "sa",
        domain: 0x00000003,
    },
    CountryName {
        code: 752,
        name: "se",
        domain: 0x00000037,
    },
    CountryName {
        code: 702,
        name: "sg",
        domain: 0x00000042,
    },
    CountryName {
        code: 705,
        name: "si",
        domain: 0x00000037,
    },
    CountryName {
        code: 703,
        name: "sk",
        domain: 0x00000036,
    },
    CountryName {
        code: 222,
        name: "sv",
        domain: 0x00000003,
    },
    CountryName {
        code: 760,
        name: "sy",
        domain: 0x00000003,
    },
    CountryName {
        code: 764,
        name: "th",
        domain: 0x00000045,
    },
    CountryName {
        code: 788,
        name: "tn",
        domain: 0x00000036,
    },
    CountryName {
        code: 792,
        name: "tr",
        domain: 0x00000036,
    },
    CountryName {
        code: 780,
        name: "tt",
        domain: 0x00000030,
    },
    CountryName {
        code: 158,
        name: "tw",
        domain: 0x00000047,
    },
    CountryName {
        code: 804,
        name: "ua",
        domain: 0x00000003,
    },
    CountryName {
        code: 826,
        name: "uk",
        domain: 0x00000037,
    },
    CountryName {
        code: 840,
        name: "us",
        domain: 0x00000010,
    },
    CountryName {
        code: 858,
        name: "uy",
        domain: 0x00000045,
    },
    CountryName {
        code: 860,
        name: "uz",
        domain: 0x0000003a,
    },
    CountryName {
        code: 862,
        name: "ve",
        domain: 0x00000056,
    },
    CountryName {
        code: 704,
        name: "vn",
        domain: 0x00000003,
    },
    CountryName {
        code: 887,
        name: "ye",
        domain: 0x00000003,
    },
    CountryName {
        code: 710,
        name: "za",
        domain: 0x00000037,
    },
    CountryName {
        code: 716,
        name: "zw",
        domain: 0x00000003,
    },
];

// upstream: ieee80211_regdomain.c bsearch()
fn binary_search<T>(items: &[T], mut compare: impl FnMut(&T) -> core::cmp::Ordering) -> Option<&T> {
    let (mut base, mut limit) = (0usize, items.len());
    while limit != 0 {
        let half = limit >> 1;
        let mid = base + half;
        match compare(&items[mid]) {
            core::cmp::Ordering::Equal => return Some(&items[mid]),
            core::cmp::Ordering::Greater => {
                base = mid + 1;
                limit -= half + 1;
            }
            core::cmp::Ordering::Less => limit = half,
        }
    }
    None
}

// upstream: ieee80211_regdomain.c ieee80211_regdomain_compare_cn()
pub fn compare_country_name(a: &str, b: &str) -> core::cmp::Ordering {
    a.cmp(b)
}
// upstream: ieee80211_regdomain.c ieee80211_regdomain_compare_rn()
pub fn compare_regdomain_name(a: &str, b: &str) -> core::cmp::Ordering {
    a.cmp(b)
}
// upstream: ieee80211_regdomain.c ieee80211_name2countrycode()
pub fn name_to_country_code(name: &str) -> u16 {
    binary_search(COUNTRY_NAMES, |v| compare_country_name(name, v.name)).map_or(0, |v| v.code)
}
// upstream: ieee80211_regdomain.c ieee80211_name2regdomain()
pub fn name_to_regdomain(name: &str) -> u32 {
    binary_search(REGDOMAIN_NAMES, |v| compare_regdomain_name(name, v.name))
        .map_or(DMN_DEFAULT, |v| v.domain)
}
// upstream: ieee80211_regdomain.c ieee80211_countrycode2name()
pub fn country_code_to_name(code: u16) -> Option<&'static str> {
    COUNTRY_NAMES
        .iter()
        .find(|v| v.code == code)
        .map(|v| v.name)
}
// upstream: ieee80211_regdomain.c ieee80211_regdomain2name()
pub fn regdomain_to_name(domain: u32) -> &'static str {
    REGDOMAIN_NAMES
        .iter()
        .find(|v| v.domain == domain)
        .map_or(REGDOMAIN_NAMES[0].name, |v| v.name)
}
// upstream: ieee80211_regdomain.c ieee80211_regdomain2flag()
pub fn regdomain_to_flag(domain: u16, mhz: u16) -> u32 {
    if let Some(row) = REGDOMAIN_MAP.iter().find(|v| v.domain == domain) {
        if (2000..=3000).contains(&mhz) {
            return row.map2;
        }
        if (CHANNELS_5GHZ_MIN..=CHANNELS_5GHZ_MAX).contains(&mhz) {
            return row.map5;
        }
    }
    DMN_DEBUG
}
// upstream: ieee80211_regdomain.c ieee80211_countrycode2regdomain()
pub fn country_code_to_regdomain(code: u16) -> u32 {
    COUNTRY_NAMES
        .iter()
        .find(|v| v.code == code)
        .map_or(DMN_DEFAULT, |v| v.domain)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn country_and_regdomain_binary_search_matches_sorted_tables() {
        assert_eq!(name_to_country_code("jp"), 392);
        assert_eq!(country_code_to_name(840), Some("us"));
        assert_eq!(name_to_regdomain("WORLD"), 0x03);
        assert_eq!(regdomain_to_name(0x10), "FCC1A");
        assert_eq!(country_code_to_regdomain(392), 0x40);
        assert_eq!(name_to_country_code("zz"), 0);
    }
    #[test]
    fn band_flags_use_openbsd_channel_ranges_and_default() {
        assert_eq!(regdomain_to_flag(0x10, 2412), 0xf002_0000);
        assert_eq!(regdomain_to_flag(0x10, 5180), 0xf000_4000);
        assert_eq!(regdomain_to_flag(0x10, 4900), DMN_DEBUG);
    }
}
