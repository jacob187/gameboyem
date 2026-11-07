//! Shared bits for the two GBA homebrew games: a 3x5 pixel font made in code
//! (no font file, no assets).
#![no_std]

pub mod font {
    /// Glyph width and height in font pixels.
    pub const WIDTH: usize = 3;
    pub const HEIGHT: usize = 5;

    /// Rows of a glyph, top to bottom; in each row bit 2 is the left pixel.
    /// Lower case maps to upper case; unknown bytes are blank.
    pub const fn rows(c: u8) -> [u8; HEIGHT] {
        match c.to_ascii_uppercase() {
            b'A' => [0b010, 0b101, 0b111, 0b101, 0b101],
            b'B' => [0b110, 0b101, 0b110, 0b101, 0b110],
            b'C' => [0b011, 0b100, 0b100, 0b100, 0b011],
            b'D' => [0b110, 0b101, 0b101, 0b101, 0b110],
            b'E' => [0b111, 0b100, 0b110, 0b100, 0b111],
            b'F' => [0b111, 0b100, 0b110, 0b100, 0b100],
            b'G' => [0b011, 0b100, 0b101, 0b101, 0b011],
            b'H' => [0b101, 0b101, 0b111, 0b101, 0b101],
            b'I' => [0b111, 0b010, 0b010, 0b010, 0b111],
            b'J' => [0b001, 0b001, 0b001, 0b101, 0b010],
            b'K' => [0b101, 0b101, 0b110, 0b101, 0b101],
            b'L' => [0b100, 0b100, 0b100, 0b100, 0b111],
            b'M' => [0b101, 0b111, 0b111, 0b101, 0b101],
            b'N' => [0b110, 0b101, 0b101, 0b101, 0b101],
            b'O' => [0b010, 0b101, 0b101, 0b101, 0b010],
            b'P' => [0b110, 0b101, 0b110, 0b100, 0b100],
            b'Q' => [0b010, 0b101, 0b101, 0b110, 0b011],
            b'R' => [0b110, 0b101, 0b110, 0b101, 0b101],
            b'S' => [0b011, 0b100, 0b010, 0b001, 0b110],
            b'T' => [0b111, 0b010, 0b010, 0b010, 0b010],
            b'U' => [0b101, 0b101, 0b101, 0b101, 0b111],
            b'V' => [0b101, 0b101, 0b101, 0b101, 0b010],
            b'W' => [0b101, 0b101, 0b111, 0b111, 0b101],
            b'X' => [0b101, 0b101, 0b010, 0b101, 0b101],
            b'Y' => [0b101, 0b101, 0b010, 0b010, 0b010],
            b'Z' => [0b111, 0b001, 0b010, 0b100, 0b111],
            b'0' => [0b111, 0b101, 0b101, 0b101, 0b111],
            b'1' => [0b010, 0b110, 0b010, 0b010, 0b111],
            b'2' => [0b110, 0b001, 0b010, 0b100, 0b111],
            b'3' => [0b110, 0b001, 0b010, 0b001, 0b110],
            b'4' => [0b101, 0b101, 0b111, 0b001, 0b001],
            b'5' => [0b111, 0b100, 0b110, 0b001, 0b110],
            b'6' => [0b011, 0b100, 0b111, 0b101, 0b111],
            b'7' => [0b111, 0b001, 0b010, 0b010, 0b010],
            b'8' => [0b111, 0b101, 0b111, 0b101, 0b111],
            b'9' => [0b111, 0b101, 0b111, 0b001, 0b110],
            b'-' => [0b000, 0b000, 0b111, 0b000, 0b000],
            b'!' => [0b010, 0b010, 0b010, 0b000, 0b010],
            b':' => [0b000, 0b010, 0b000, 0b010, 0b000],
            b'.' => [0b000, 0b000, 0b000, 0b000, 0b010],
            _ => [0; HEIGHT],
        }
    }

    /// Decimal digits of `v` (most significant first), zero padded.
    pub const fn digits<const N: usize>(mut v: u32) -> [u8; N] {
        let mut out = [b'0'; N];
        let mut i = N;
        while i > 0 {
            i -= 1;
            out[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        out
    }
}
