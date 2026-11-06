//! Pure arithmetic of the HLE BIOS: Div, Sqrt, ArcTan, ArcTan2, the sine table
//! and MidiKey2Freq. No bus access, no floats (so native and wasm agree).
//!
//! Provenance (DECISIONS #64): GBATEK ("BIOS Arithmetic Functions") specifies
//! the registers, ranges and the Div example only. It gives neither the ArcTan
//! polynomial nor the sine table, so those come from the documented BIOS
//! algorithms (the table is `trunc(0x4000 * sin(2 pi i / 256))`, the first
//! sixteen entries match the values quoted in homebrew documentation, and the
//! ArcTan coefficients telescope so that tan = 1.0 gives exactly 0x2000; both
//! facts are tests below).

/// Quarter wave of the BIOS sine table: `trunc(0x4000 * sin(2 pi i / 256))` for
/// `i = 0..=64`, generated once offline with exact arithmetic.
const QUARTER: [i16; 65] = [
    0x0000, 0x0192, 0x0323, 0x04B5, 0x0645, 0x07D5, 0x0964, 0x0AF1, 0x0C7C, 0x0E05, 0x0F8C, 0x1111,
    0x1294, 0x1413, 0x158F, 0x1708, 0x187D, 0x19EF, 0x1B5D, 0x1CC6, 0x1E2B, 0x1F8B, 0x20E7, 0x223D,
    0x238E, 0x24DA, 0x261F, 0x275F, 0x2899, 0x29CD, 0x2AFA, 0x2C21, 0x2D41, 0x2E5A, 0x2F6B, 0x3076,
    0x3179, 0x3274, 0x3367, 0x3453, 0x3536, 0x3612, 0x36E5, 0x37AF, 0x3871, 0x392A, 0x39DA, 0x3A82,
    0x3B20, 0x3BB6, 0x3C42, 0x3CC5, 0x3D3E, 0x3DAE, 0x3E14, 0x3E71, 0x3EC5, 0x3F0E, 0x3F4E, 0x3F84,
    0x3FB1, 0x3FD3, 0x3FEC, 0x3FFB, 0x4000,
];

/// The full circle: `SINE[i] = sin(2 pi i / 256)` in 1.14 fixed point.
pub const SINE: [i16; 256] = {
    let mut t = [0i16; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = match i {
            0..=63 => QUARTER[i],
            64..=127 => QUARTER[128 - i],
            128..=191 => -QUARTER[i - 128],
            _ => -QUARTER[256 - i],
        };
        i += 1;
    }
    t
};

/// `(sin, cos)` of the angle `theta` (0..=0xFFFF = a full turn; only the upper
/// 8 bits count, GBATEK "BIOS Rotation/Scaling Functions"), 1.14 fixed point.
pub fn sin_cos(theta: u16) -> (i32, i32) {
    let i = usize::from(theta >> 8);
    (i32::from(SINE[i]), i32::from(SINE[(i + 64) & 0xFF]))
}

/// Div: `(quotient, remainder, |quotient|)`; `None` for a zero denominator (the
/// BIOS loops forever, GBATEK "BIOS Arithmetic Functions").
pub fn div(num: i32, den: i32) -> Option<(i32, i32, u32)> {
    if den == 0 {
        return None;
    }
    let q = num.wrapping_div(den);
    Some((q, num.wrapping_rem(den), q.unsigned_abs()))
}

/// Sqrt: the integer square root, rounded down.
pub fn sqrt(n: u32) -> u32 {
    let mut r = 0u32;
    let mut bit = 1u32 << 30;
    let mut n = n;
    while bit > n {
        bit >>= 2;
    }
    while bit != 0 {
        if n >= r + bit {
            n -= r + bit;
            r = (r >> 1) + bit;
        } else {
            r >>= 1;
        }
        bit >>= 2;
    }
    r
}

/// ArcTan of a 1.14 fixed-point tangent: a degree-7 polynomial in `-i^2`.
/// Returns an angle in 0x4000 = a quarter turn units (so tan 1.0 = 0x2000).
pub fn arctan(i: i32) -> i32 {
    let a = -(i.wrapping_mul(i) >> 14);
    let mut b = (0xA9i32.wrapping_mul(a) >> 14) + 0x390;
    for c in [0x91C, 0xFB6, 0x16AA, 0x2081, 0x3651, 0xA2F9] {
        b = (b.wrapping_mul(a) >> 14) + c;
    }
    i.wrapping_mul(b) >> 16
}

/// ArcTan2 of 1.14 fixed-point `x`, `y`: 0..=0xFFFF for 0 <= theta < 2 pi. The
/// polynomial is only accurate within +-45 degrees, so the other octants use
/// the complementary ratio.
pub fn arctan2(x: i32, y: i32) -> u32 {
    let r = if y == 0 {
        if x >= 0 { 0 } else { 0x8000 }
    } else if x == 0 {
        if y >= 0 { 0x4000 } else { 0xC000 }
    } else if y >= 0 {
        if x >= 0 {
            if x >= y {
                arctan((y << 14) / x)
            } else {
                0x4000 - arctan((x << 14) / y)
            }
        } else if -x >= y {
            arctan((y << 14) / x) + 0x8000
        } else {
            0x4000 - arctan((x << 14) / y)
        }
    } else if x <= 0 {
        if -x > -y {
            arctan((y << 14) / x) + 0x8000
        } else {
            0xC000 - arctan((x << 14) / y)
        }
    } else if x >= -y {
        arctan((y << 14) / x) + 0x10000
    } else {
        0xC000 - arctan((x << 14) / y)
    };
    r as u32 & 0xFFFF
}

/// 2^(-k/12) in 0.32 fixed point for k = 0..=12 (semitone ratios).
const SEMITONE: [u64; 13] = [
    0x1_0000_0000,
    0xF1A1_BF38,
    0xE411_F03A,
    0xD744_FCCA,
    0xCB2F_F529,
    0xBFC8_86BB,
    0xB504_F333,
    0xAADC_0847,
    0xA145_17CC,
    0x9837_F051,
    0x8FAC_D61E,
    0x879C_7C96,
    0x8000_0000,
];

/// MidiKey2Freq (APPROXIMATION): `freq / 2^((180 - mk - fp/256) / 12)`, the
/// formula GBATEK's WaveData description gives ("sampling rate x 2^((180-
/// original MIDI key)/12)"), evaluated with a semitone table and linear
/// interpolation between semitones. The real BIOS uses its own tables, so the
/// low bits can differ.
pub fn midi_key_to_freq(freq: u32, mk: u8, fp: u8) -> u32 {
    let e = (180 - i64::from(mk)) * 256 - i64::from(fp);
    let octave = e.div_euclid(3072);
    let rem = e.rem_euclid(3072);
    let (k, frac) = ((rem / 256) as usize, (rem % 256) as u64);
    let (hi, lo) = (SEMITONE[k], SEMITONE[k + 1]);
    let factor = hi - (hi - lo) * frac / 256;
    let v = (u64::from(freq) * factor) >> 32;
    let v = if octave >= 0 {
        v >> octave.min(63)
    } else {
        v << (-octave).min(32)
    };
    v as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_table_symmetries_and_known_entries() {
        assert_eq!(&SINE[..4], &[0, 0x192, 0x323, 0x4B5]);
        assert_eq!(SINE[64], 0x4000);
        assert_eq!(SINE[128], 0);
        assert_eq!(SINE[192], -0x4000);
        assert_eq!(SINE[255], -0x192);
        assert_eq!(SINE[100], SINE[28]);
        assert_eq!(sin_cos(0), (0, 0x4000));
        assert_eq!(sin_cos(0x4000), (0x4000, 0));
    }

    #[test]
    fn div_follows_gbatek_example_and_signs() {
        // GBATEK: -1234 / 10 -> -123, -4, +123.
        assert_eq!(div(-1234, 10), Some((-123, -4, 123)));
        assert_eq!(div(1234, -10), Some((-123, 4, 123)));
        assert_eq!(div(-7, -2), Some((3, -1, 3)));
        assert_eq!(div(5, 7), Some((0, 5, 0)));
        assert_eq!(div(i32::MIN, -1), Some((i32::MIN, 0, 0x8000_0000)));
        assert_eq!(div(1, 0), None);
    }

    #[test]
    fn sqrt_is_floor_sqrt() {
        for n in [0u32, 1, 2, 3, 4, 15, 16, 17, 99, 100, 65535, 65536] {
            let r = u64::from(sqrt(n));
            assert!(
                r * r <= u64::from(n) && (r + 1) * (r + 1) > u64::from(n),
                "{n}"
            );
        }
        assert_eq!(sqrt(u32::MAX), 65535);
        assert_eq!(sqrt(0xFFFF_0000), 65535);
        assert_eq!(sqrt(2 << 30), 46340); // GBATEK: 1.41421 << 15
    }

    #[test]
    fn arctan_polynomial_is_pinned() {
        // tan = 1.0 -> pi/4 = 0x2000 exactly (the coefficients telescope).
        assert_eq!(arctan(0x4000), 0x2000);
        assert_eq!(arctan(0), 0);
        // Odd symmetry up to the arithmetic shift's rounding.
        assert!((arctan(-0x2000) + arctan(0x2000)).abs() <= 1);
        // atan(0.5) = 0.4636 rad = 0x12E4 (0x10000 = 2 pi).
        let v = arctan(0x2000);
        assert!((v - 0x12E4).abs() < 0x20, "{v:#x}");
    }

    #[test]
    fn arctan2_covers_the_four_quadrants() {
        let u = 0x4000;
        assert_eq!(arctan2(u, 0), 0);
        assert_eq!(arctan2(0, u), 0x4000);
        assert_eq!(arctan2(-u, 0), 0x8000);
        assert_eq!(arctan2(0, -u), 0xC000);
        assert_eq!(arctan2(u, u), 0x2000);
        assert_eq!(arctan2(-u, u), 0x6000);
        assert_eq!(arctan2(-u, -u), 0xA000);
        assert_eq!(arctan2(u, -u), 0xE000);
        // Steep angles use the complementary ratio.
        let v = arctan2(0x1000, 0x4000) as i32;
        assert!((v - 0x3605).abs() < 0x40, "{v:#x}"); // atan(4) = 1.3258 rad
    }

    #[test]
    fn midi_key_is_an_octave_per_twelve_keys() {
        assert_eq!(midi_key_to_freq(1 << 20, 180, 0), 1 << 20);
        assert_eq!(midi_key_to_freq(1 << 20, 168, 0), 1 << 19);
        assert_eq!(midi_key_to_freq(1 << 20, 192, 0), 1 << 21);
        assert_eq!(midi_key_to_freq(1 << 20, 174, 0), 741_455); // 2^20 / sqrt(2)
    }
}
