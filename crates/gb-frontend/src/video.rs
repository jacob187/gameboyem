//! Framebuffer presentation: DMG shade or CGB RGB555 -> RGBA and a synthetic test pattern.

use gb_core::{CgbFramebuffer, Framebuffer, SCREEN_HEIGHT, SCREEN_WIDTH, rgb555_to_rgb888};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteKind {
    Green,
    Grey,
}

/// RGBA for shades 0 (lightest) ..= 3 (darkest).
pub type Palette = [[u8; 4]; 4];

impl PaletteKind {
    pub fn colors(self) -> Palette {
        match self {
            // The widely used "DMG green" ramp.
            PaletteKind::Green => [
                [0xE0, 0xF8, 0xD0, 0xFF],
                [0x88, 0xC0, 0x70, 0xFF],
                [0x34, 0x68, 0x56, 0xFF],
                [0x08, 0x18, 0x20, 0xFF],
            ],
            PaletteKind::Grey => [
                [0xFF, 0xFF, 0xFF, 0xFF],
                [0xAA, 0xAA, 0xAA, 0xFF],
                [0x55, 0x55, 0x55, 0xFF],
                [0x00, 0x00, 0x00, 0xFF],
            ],
        }
    }
}

/// Convert shades to RGBA8 in `out` (`160*144*4` bytes). Shades above 3 are masked.
pub fn blit(fb: &Framebuffer, palette: &Palette, out: &mut [u8]) {
    assert_eq!(out.len(), fb.len() * 4, "blit needs a 160x144 RGBA buffer");
    let (pixels, _) = out.as_chunks_mut::<4>();
    for (shade, px) in fb.iter().zip(pixels) {
        *px = palette[usize::from(shade & 3)];
    }
}

/// Convert an RGB555 (`0bbbbbgggggrrrrr`) colour frame to RGBA8 in `out`
/// (`160*144*4` bytes). Channels widen with [`gb_core::rgb555_to_rgb888`], with
/// no colour correction (the cgb-acid2 rule); the DMG palette is not used.
pub fn blit_cgb(fb: &CgbFramebuffer, out: &mut [u8]) {
    assert_eq!(
        out.len(),
        fb.len() * 4,
        "blit_cgb needs a 160x144 RGBA buffer"
    );
    let (pixels, _) = out.as_chunks_mut::<4>();
    for (&px, out) in fb.iter().zip(pixels) {
        let [r, g, b] = rgb555_to_rgb888(px);
        *out = [r, g, b, 0xFF];
    }
}

/// Fill `fb` with a test pattern: four shade bars, a 1px frame, a shade ramp
/// along the top and a moving marker column, so scaling, orientation and
/// animation are all visible.
pub fn test_pattern(frame: u32, fb: &mut Framebuffer) {
    let marker = (frame as usize * 2) % SCREEN_WIDTH;
    for y in 0..SCREEN_HEIGHT {
        for x in 0..SCREEN_WIDTH {
            let bar = (x * 4 / SCREEN_WIDTH) as u8;
            let edge = x == 0 || y == 0 || x == SCREEN_WIDTH - 1 || y == SCREEN_HEIGHT - 1;
            fb[y * SCREEN_WIDTH + x] = if edge {
                3
            } else if x == marker && y > 8 {
                3 - bar
            } else if y < 8 {
                ((x / 8) % 4) as u8
            } else {
                bar
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_maps_shades_and_masks() {
        let pal = PaletteKind::Grey.colors();
        let mut fb = [0u8; SCREEN_WIDTH * SCREEN_HEIGHT];
        fb[1] = 1;
        fb[2] = 2;
        fb[3] = 3;
        fb[4] = 7; // masked to 3
        let mut out = vec![0u8; SCREEN_WIDTH * SCREEN_HEIGHT * 4];
        blit(&fb, &pal, &mut out);
        assert_eq!(&out[0..4], &[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(&out[4..8], &[0xAA, 0xAA, 0xAA, 0xFF]);
        assert_eq!(&out[8..12], &[0x55, 0x55, 0x55, 0xFF]);
        assert_eq!(&out[12..16], &[0, 0, 0, 0xFF]);
        assert_eq!(&out[16..20], &[0, 0, 0, 0xFF]);
    }

    #[test]
    fn blit_cgb_widens_rgb555_without_correction() {
        let mut fb = [0u16; SCREEN_WIDTH * SCREEN_HEIGHT];
        fb[1] = 0x7FFF;
        fb[2] = 0x001F; // red is the low 5 bits
        fb[3] = 0x7C00; // blue the high
        fb[4] = 0b01111_00001_10000;
        fb[5] = 0x8000; // bit 15 ignored
        let mut out = vec![0x11u8; SCREEN_WIDTH * SCREEN_HEIGHT * 4];
        blit_cgb(&fb, &mut out);
        assert_eq!(&out[0..4], &[0, 0, 0, 0xFF]);
        assert_eq!(&out[4..8], &[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(&out[8..12], &[0xFF, 0, 0, 0xFF]);
        assert_eq!(&out[12..16], &[0, 0, 0xFF, 0xFF]);
        assert_eq!(&out[16..20], &[0x84, 0x08, 0x7B, 0xFF]);
        assert_eq!(&out[20..24], &[0, 0, 0, 0xFF]);
    }

    #[test]
    #[should_panic(expected = "blit needs")]
    fn blit_rejects_a_wrong_sized_buffer() {
        let fb = [0u8; SCREEN_WIDTH * SCREEN_HEIGHT];
        blit(&fb, &PaletteKind::Grey.colors(), &mut [0u8; 16]);
    }

    #[test]
    #[should_panic(expected = "blit_cgb needs")]
    fn blit_cgb_rejects_a_wrong_sized_buffer() {
        let fb = [0u16; SCREEN_WIDTH * SCREEN_HEIGHT];
        blit_cgb(&fb, &mut vec![0u8; SCREEN_WIDTH * SCREEN_HEIGHT * 4 - 4]);
    }

    #[test]
    fn palettes_run_light_to_dark() {
        for kind in [PaletteKind::Green, PaletteKind::Grey] {
            let p = kind.colors();
            let lum = |c: [u8; 4]| u32::from(c[0]) + u32::from(c[1]) + u32::from(c[2]);
            assert!(lum(p[0]) > lum(p[1]) && lum(p[1]) > lum(p[2]) && lum(p[2]) > lum(p[3]));
            assert!(p.iter().all(|c| c[3] == 0xFF));
        }
    }

    #[test]
    fn test_pattern_is_valid_and_animates() {
        let mut a = [0u8; SCREEN_WIDTH * SCREEN_HEIGHT];
        let mut b = a;
        test_pattern(0, &mut a);
        test_pattern(1, &mut b);
        assert!(a.iter().all(|&s| s <= 3));
        assert_ne!(a, b);
    }
}
