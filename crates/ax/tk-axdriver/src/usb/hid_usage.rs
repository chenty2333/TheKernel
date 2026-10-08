//! HID usage facts mapped to Linux evdev identifiers. Original implementation;
//! behavior reference: Linux 7.2.3 hid-input.c and input-event-codes.h, USB HUT 1.5.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Usage {
    pub page: u32,
    pub code: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mapping {
    Key(u16),
    Axis(u16, u16),
    Hat,
    Wheel(u16),
}
fn keyboard(usage: u32) -> Option<u16> {
    const KEYS: [u16; 100] = [
        0, 0, 0, 0, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20,
        22, 47, 17, 45, 21, 44, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 28, 1, 14, 15, 57, 12, 13, 26, 27,
        43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88, 99, 70,
        119, 110, 102, 104, 111, 107, 109, 106, 105, 108, 103, 69, 98, 55, 74, 78, 96, 79, 80, 81,
        75, 76, 77, 71, 72, 73, 82, 83,
    ];
    let code = match usage {
        100 => 86,
        101 => 127,
        102 => 116,
        103 => 117,
        104..=115 => 183 + (usage - 104) as u16,
        116 => 134,
        117 => 138,
        118 => 130,
        119 => 132,
        120 => 128,
        121 => 129,
        122 => 131,
        123 => 137,
        124 => 133,
        125 => 135,
        126 => 136,
        127 => 113,
        128 => 115,
        129 => 114,
        133 => 121,
        135 => 89,
        136 => 93,
        137 => 124,
        138 => 92,
        139 => 94,
        140 => 95,
        144 => 122,
        145 => 123,
        146 => 90,
        147 => 91,
        148 => 85,
        156 => 111,
        182 => 179,
        183 => 180,
        216 => 111,
        224..=231 => [29, 42, 56, 125, 97, 54, 100, 126][(usage - 224) as usize],
        232..=239 => [164, 166, 165, 163, 161, 115, 114, 113][(usage - 232) as usize],
        240..=251 => {
            [150, 158, 159, 128, 136, 177, 178, 176, 142, 152, 173, 140][(usage - 240) as usize]
        }
        _ => KEYS.get(usage as usize).copied().unwrap_or(0),
    };
    (code != 0).then_some(code)
}
fn consumer(code: u32) -> Option<u16> {
    // A compact subset of standard Consumer controls. Unmapped usages do not
    // synthesize KEY_UNKNOWN or create phantom keyboard events.
    Some(match code {
        0x30 => 116,
        0x31 => 0x198,
        0x32 | 0x34 => 142,
        0x35 | 0x7c => 228,
        0x40 => 139,
        0x41 => 0x161,
        0x42 => 103,
        0x43 => 108,
        0x44 => 105,
        0x45 => 106,
        0x46 => 1,
        0x47 => 78,
        0x48 => 74,
        0x6f => 225,
        0x70 => 224,
        0x79 => 230,
        0x7a => 229,
        0xb0 => 207,
        0xb1 => 119,
        0xb2 => 167,
        0xb3 => 208,
        0xb4 => 168,
        0xb5 => 163,
        0xb6 => 165,
        0xb7 => 166,
        0xb8 => 161,
        0xcd => 164,
        0xe2 => 113,
        0xe9 => 115,
        0xea => 114,
        0x182 => 156,
        0x183 => 171,
        0x18a => 155,
        0x192 => 140,
        0x196 => 150,
        0x221 => 217,
        0x223 => 172,
        0x224 => 158,
        0x225 => 159,
        0x227 => 173,
        0x22a => 156,
        _ => return None,
    })
}
pub(super) fn mapping(usage: Usage, application: Usage, relative: bool) -> Option<Mapping> {
    match usage.page {
        7 => keyboard(usage.code).map(Mapping::Key),
        9 if (1..=128).contains(&usage.code) => {
            let index = (usage.code - 1) as u16;
            let base = if application.page == 1 {
                match application.code {
                    1 | 2 => 0x110,
                    4 => 0x120,
                    5 => 0x130,
                    _ => 0x100,
                }
            } else {
                0x100
            };
            let code = if matches!(base, 0x120 | 0x130) && index >= 16 {
                0x2c0 + index - 16
            } else {
                base + index
            };
            (code < 0x300).then_some(Mapping::Key(code))
        }
        1 if usage.code == 0x38 && relative => Some(Mapping::Wheel(8)),
        1 if (0x30..=0x38).contains(&usage.code) => Some(Mapping::Axis(
            if relative { 2 } else { 3 },
            (usage.code - 0x30) as u16,
        )),
        1 if usage.code == 0x39 && !relative => Some(Mapping::Hat),
        1 if usage.code == 0x3d => Some(Mapping::Key(0x13b)), // Start
        1 if usage.code == 0x3e => Some(Mapping::Key(0x13a)), // Select
        12 if usage.code == 0x238 => Some(Mapping::Wheel(6)), // horizontal scroll
        12 if usage.code == 0xe0 => Some(Mapping::Axis(3, 0x20)),
        12 => consumer(usage.code).map(Mapping::Key),
        0x0d => match usage.code {
            0x30 => Some(Mapping::Axis(3, 0x35)), // ABS_MT_POSITION_X
            0x31 => Some(Mapping::Axis(3, 0x36)), // ABS_MT_POSITION_Y
            0x42 => Some(Mapping::Key(0x14a)),    // BTN_TOUCH
            0x32 => Some(Mapping::Key(0x145)),    // BTN_TOOL_FINGER
            0x48 => Some(Mapping::Axis(3, 0x30)), // ABS_MT_TOUCH_MAJOR
            0x49 => Some(Mapping::Axis(3, 0x31)), // ABS_MT_TOUCH_MINOR
            0x51 => Some(Mapping::Axis(3, 0x39)), // ABS_MT_TRACKING_ID
            0x54 => Some(Mapping::Axis(3, 0x3a)), // ABS_MT_PRESSURE
            _ => None,
        },
        _ => None,
    }
}
