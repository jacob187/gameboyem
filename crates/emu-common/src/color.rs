//! Pixel format helpers shared by the cores and the frontend.

/// Widen one RGB555 pixel (`0bbbbbgggggrrrrr`, red in the low 5 bits; bit 15
/// is ignored) to RGB888. Each 5-bit channel `c` becomes `(c << 3) | (c >> 2)`,
/// with no colour correction (the rule the cgb-acid2 reference image uses).
pub const fn rgb555_to_rgb888(c: u16) -> [u8; 3] {
    const fn widen(c: u16) -> u8 {
        let c = c & 0x1F;
        ((c << 3) | (c >> 2)) as u8
    }
    [widen(c), widen(c >> 5), widen(c >> 10)]
}

#[cfg(test)]
mod tests {
    use super::rgb555_to_rgb888;

    #[test]
    fn rgb555_expands_with_the_readme_rule() {
        assert_eq!(rgb555_to_rgb888(0x0000), [0, 0, 0]);
        assert_eq!(rgb555_to_rgb888(0x7FFF), [255, 255, 255]);
        // Red is the low 5 bits, blue the high.
        assert_eq!(rgb555_to_rgb888(0x001F), [255, 0, 0]);
        assert_eq!(rgb555_to_rgb888(0x03E0), [0, 255, 0]);
        assert_eq!(rgb555_to_rgb888(0x7C00), [0, 0, 255]);
        // (c << 3) | (c >> 2): 0x10 -> 0x84, 0x01 -> 0x08, 0x0F -> 0x7B.
        assert_eq!(rgb555_to_rgb888(0b01111_00001_10000), [0x84, 0x08, 0x7B]);
        // Bit 15 is ignored.
        assert_eq!(rgb555_to_rgb888(0x8000), [0, 0, 0]);
    }
}
