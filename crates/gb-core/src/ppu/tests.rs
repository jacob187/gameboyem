//! PPU unit tests: the `Ppu` is driven directly with synthetic VRAM / OAM /
//! register state, no CPU or cartridge involved.

use super::*;
use crate::M_CYCLES_PER_FRAME;

const STAT_BIT: u8 = Interrupt::Stat as u8;
const VBLANK_BIT: u8 = Interrupt::VBlank as u8;

/// A PPU plus its interrupt block, with identity palettes and LCD on.
struct Rig {
    ppu: Ppu,
    ints: Interrupts,
}

impl Rig {
    fn new() -> Self {
        let mut ppu = Ppu::new();
        ppu.bgp = 0xE4;
        ppu.obp0 = 0xE4;
        ppu.obp1 = 0x1B;
        Self {
            ppu,
            ints: Interrupts::default(),
        }
    }

    /// Rig positioned at dot 0 of line 0 (mode 2) of a normal (non-first) frame.
    fn at_line0() -> Self {
        let mut rig = Self::new();
        rig.run_to(0, 0);
        rig
    }

    fn tick(&mut self) {
        self.ppu.tick_mcycle(&mut self.ints);
    }

    fn ticks(&mut self, n: u32) {
        for _ in 0..n {
            self.tick();
        }
    }

    /// Tick until the PPU is at (`ly`, `dot`); `dot` must be a multiple of 4.
    fn run_to(&mut self, ly: u8, dot: u16) {
        let mut guard = 0u32;
        while !(self.ppu.ly == ly && self.ppu.line_dot == dot) {
            self.tick();
            guard += 1;
            assert!(
                guard < 2 * M_CYCLES_PER_FRAME + 10,
                "never reached ly={ly} dot={dot}"
            );
        }
    }

    /// Run to the end of the current frame and return the completed framebuffer.
    fn frame(&mut self) -> Vec<u8> {
        let mut guard = 0u32;
        while !self.ppu.take_frame_ready() {
            self.tick();
            guard += 1;
            assert!(guard < 2 * M_CYCLES_PER_FRAME + 10, "no frame completed");
        }
        self.ppu.framebuffer().to_vec()
    }

    fn write(&mut self, addr: u16, value: u8) {
        self.ppu.write_reg(addr, value, &mut self.ints);
    }

    fn clear_if(&mut self) {
        self.ints.flags = 0;
    }

    /// Fill the BG tile map at `map_off` (0x1800 or 0x1C00) with one tile.
    fn fill_map(&mut self, map_off: usize, tile: u8) {
        self.ppu.vram[map_off..map_off + 0x400].fill(tile);
    }

    /// Write one tile (8 rows of 8 colour ids) at VRAM offset `off`.
    fn set_tile(&mut self, off: usize, px: [[u8; 8]; 8]) {
        for (r, row) in px.iter().enumerate() {
            let (mut lo, mut hi) = (0u8, 0u8);
            for (c, &id) in row.iter().enumerate() {
                lo |= (id & 1) << (7 - c);
                hi |= ((id >> 1) & 1) << (7 - c);
            }
            self.ppu.vram[off + r * 2] = lo;
            self.ppu.vram[off + r * 2 + 1] = hi;
        }
    }

    fn solid_tile(&mut self, off: usize, id: u8) {
        self.set_tile(off, [[id; 8]; 8]);
    }

    fn set_sprite(&mut self, index: usize, y: u8, x: u8, tile: u8, flags: u8) {
        self.ppu.oam[index * 4..index * 4 + 4].copy_from_slice(&[y, x, tile, flags]);
    }
}

fn pixel(fb: &[u8], x: usize, y: usize) -> u8 {
    fb[y * SCREEN_WIDTH + x]
}

// ---------------------------------------------------------------- timing

#[test]
fn power_on_state_matches_dmg_boot() {
    let ppu = Ppu::new();
    assert_eq!(ppu.read_reg(0xFF40), 0x91);
    assert_eq!(ppu.read_reg(0xFF41), 0x85);
    assert_eq!(ppu.read_reg(0xFF44), 0x00);
    assert_eq!(ppu.read_reg(0xFF47), 0xFC);
}

#[test]
fn mode_sequence_and_dot_counts_on_a_visible_line() {
    let mut rig = Rig::at_line0();
    let mut counts = [0u32; 4];
    let mut order = Vec::new();
    for _ in 0..(456 / 4) {
        let mode = rig.ppu.mode();
        counts[usize::from(mode)] += 4;
        if order.last() != Some(&mode) {
            order.push(mode);
        }
        rig.tick();
    }
    assert_eq!(order, [2, 3, 0]);
    assert_eq!(counts, [204, 0, 80, 172]);
    // After a full line we are at the start of line 1, mode 2 again.
    assert_eq!((rig.ppu.ly, rig.ppu.line_dot, rig.ppu.mode()), (1, 0, 2));
}

#[test]
fn lines_144_to_153_are_vblank() {
    let mut rig = Rig::at_line0();
    for ly in 0..154u8 {
        let expect_vblank = ly >= 144;
        for _ in 0..114 {
            assert_eq!(rig.ppu.ly, ly);
            assert_eq!(rig.ppu.mode() == 1, expect_vblank, "ly {ly}");
            rig.tick();
        }
    }
    assert_eq!(rig.ppu.ly, 0);
}

#[test]
fn vblank_interrupt_and_frame_ready_every_70224_dots() {
    let mut rig = Rig::at_line0();
    rig.clear_if();
    let mut vblanks = 0;
    let mut gaps = Vec::new();
    let mut since = 0u32;
    for _ in 0..(3 * M_CYCLES_PER_FRAME) {
        rig.tick();
        since += 4;
        if rig.ints.flags & VBLANK_BIT != 0 {
            vblanks += 1;
            rig.clear_if();
        }
        if rig.ppu.take_frame_ready() {
            gaps.push(since);
            since = 0;
        }
    }
    assert_eq!(vblanks, 3);
    assert_eq!(gaps.len(), 3);
    // Frames are exactly 70224 dots apart (the first gap starts at line 0).
    assert_eq!(gaps[1], 70_224);
    assert_eq!(gaps[2], 70_224);
}

#[test]
fn vblank_requested_exactly_when_ly_reaches_144() {
    let mut rig = Rig::at_line0();
    rig.clear_if();
    rig.run_to(143, 452);
    assert_eq!(rig.ints.flags & VBLANK_BIT, 0);
    rig.tick();
    assert_eq!(rig.ppu.ly, 144);
    assert_ne!(rig.ints.flags & VBLANK_BIT, 0);
}

#[test]
fn ly_reads_zero_early_on_line_153() {
    let mut rig = Rig::at_line0();
    rig.run_to(153, 0);
    assert_eq!(rig.ppu.read_reg(0xFF44), 153);
    rig.tick();
    assert_eq!(rig.ppu.read_reg(0xFF44), 0);
}

#[test]
fn mode3_is_extended_by_scx_window_and_sprites() {
    let mut rig = Rig::at_line0();
    rig.run_to(5, 76);
    rig.tick();
    assert_eq!(rig.ppu.mode3_len, 172);

    rig.write(0xFF43, 5);
    rig.run_to(6, 76);
    rig.tick();
    assert_eq!(rig.ppu.mode3_len, 177);
    let dot = rig.ppu.line_dot;
    rig.ppu.line_dot = 80 + 176;
    assert_eq!(rig.ppu.mode(), 3);
    rig.ppu.line_dot = 80 + 177;
    assert_eq!(rig.ppu.mode(), 0);
    rig.ppu.line_dot = dot;

    // Window adds 6.
    rig.write(0xFF43, 0);
    rig.write(0xFF40, 0xB1);
    rig.write(0xFF4A, 0);
    rig.write(0xFF4B, 7);
    rig.run_to(7, 76);
    rig.tick();
    assert_eq!(rig.ppu.mode3_len, 178);

    // One sprite at OAM X=8 (alignment 0) adds 6 + 5, less the 3-dot phase.
    rig.write(0xFF40, 0x93);
    rig.set_sprite(0, 16 + 8, 8, 0, 0);
    rig.run_to(8, 76);
    rig.tick();
    assert_eq!(rig.ppu.mode3_len, 172 + 8);

    // A second sprite in another tile adds another 6 + 5.
    rig.set_sprite(1, 16 + 8, 16, 0, 0);
    rig.run_to(9, 76);
    rig.tick();
    assert_eq!(rig.ppu.mode3_len, 172 + 19);

    // Sprites at X >= 168 take an OAM slot but cost nothing.
    rig.set_sprite(1, 16 + 8, 168, 0, 0);
    rig.run_to(10, 76);
    rig.tick();
    assert_eq!(rig.ppu.mode3_len, 172 + 8);
}

/// Mode 3 length for sprites at the given OAM X values on line 8 (Y = 24),
/// with `scx` and OBJ enabled/disabled, straight from the OAM scan.
fn mode3_len_with_sprites(xs: &[u8], scx: u8, obj: bool) -> u16 {
    let mut rig = Rig::new();
    for (i, &x) in xs.iter().enumerate() {
        rig.set_sprite(i, 16 + 8, x, 0, 0);
    }
    rig.ppu.ly = 8;
    rig.ppu.scx = scx;
    rig.ppu.lcdc = if obj { 0x93 } else { 0x91 };
    rig.ppu.scan_oam();
    rig.ppu.compute_mode3_len()
}

#[test]
fn sprite_penalty_table() {
    // Per sprite: 6 dots, plus `5 - x % 8` (floored at 0) for the first sprite
    // in each BG tile (x = OAM X + SCX); the sum is then reduced by 3. These
    // are the values the mooneye-validated model (DECISIONS.md #20) produces.
    const BASE: u16 = 172;
    let table: [(&[u8], u8, u16); 13] = [
        // One sprite per alignment: 11, 10, 9, 8, 7, 6 (clamp), 6, 6 minus 3.
        (&[8], 0, BASE + 8),
        (&[9], 0, BASE + 7),
        (&[12], 0, BASE + 4),
        (&[13], 0, BASE + 3),
        (&[15], 0, BASE + 3),
        (&[0], 0, BASE + 8),
        // Same BG tile: the second sprite pays 6 only (11 + 6 - 3).
        (&[8, 12], 0, BASE + 14),
        // Different tiles: both pay the alignment term (11 + 11 - 3).
        (&[8, 16], 0, BASE + 19),
        // Same tile, later sprite less aligned than the first: still dedupes.
        (&[9, 8], 0, BASE + 14),
        // SCX moves a sprite across a tile boundary: 4 and 11 are two tiles
        // (7 + 8 - 3) but 8 and 15 (SCX = 4) share one (11 + 6 - 3, +4 SCX).
        (&[4, 11], 0, BASE + 12),
        (&[4, 11], 4, BASE + 4 + 14),
        // Ten sprites: one tile (6 * 10 + 5 - 3) and ten tiles (110 - 3).
        (&[8; 10], 0, BASE + 62),
        (&[8, 16, 24, 32, 40, 48, 56, 64, 72, 80], 0, BASE + 107),
    ];
    for (xs, scx, want) in table {
        assert_eq!(
            mode3_len_with_sprites(xs, scx, true),
            want,
            "OAM X {xs:?}, SCX {scx}"
        );
    }
    // The 11th sprite on a line is not selected, so it costs nothing.
    assert_eq!(mode3_len_with_sprites(&[8; 12], 0, true), BASE + 62);
    // Sprites at X >= 168 cost nothing, wherever they sit in the list.
    assert_eq!(mode3_len_with_sprites(&[168, 8, 255], 0, true), BASE + 8);
    // OBJ disabled: no penalty at all.
    assert_eq!(mode3_len_with_sprites(&[8, 16], 0, false), BASE);
}

#[test]
fn first_line_after_lcd_on_has_no_oam_scan_or_sprite_penalty() {
    let mut rig = Rig::at_line0();
    rig.write(0xFF40, 0x93);
    for i in 0..10 {
        rig.set_sprite(i, 16, 8 + 8 * i as u8, 0, 0);
    }
    rig.write(0xFF40, 0x13);
    rig.write(0xFF40, 0x93);
    assert!(rig.ppu.first_line);
    rig.ticks(25); // past dot 80 of line 0
    assert_eq!(rig.ppu.ly, 0);
    assert_eq!(rig.ppu.line_sprite_count, 0);
    assert_eq!(rig.ppu.mode3_len, 172);
    // Line 1 scans OAM normally: the same sprites (Y = 16 spans lines 0..8).
    rig.run_to(1, 84);
    assert_eq!(rig.ppu.line_sprite_count, 10);
    assert_eq!(rig.ppu.mode3_len, 172 + 107);
}

// ------------------------------------------------------- LY/LYC and STAT

#[test]
fn lyc_coincidence_flag_and_interrupt() {
    let mut rig = Rig::at_line0();
    rig.write(0xFF45, 5);
    rig.write(0xFF41, 0x40);
    rig.clear_if();
    rig.run_to(4, 452);
    assert_eq!(rig.ppu.read_reg(0xFF41) & 0x04, 0);
    assert_eq!(rig.ints.flags & STAT_BIT, 0);
    rig.tick(); // LY becomes 5; the STAT bit stays blanked for one M-cycle
    assert_eq!(rig.ppu.ly, 5);
    assert_eq!(rig.ppu.read_reg(0xFF41) & 0x04, 0);
    assert_ne!(rig.ints.flags & STAT_BIT, 0);
    rig.tick();
    assert_eq!(rig.ppu.read_reg(0xFF41) & 0x04, 0x04);
    assert_ne!(rig.ints.flags & STAT_BIT, 0);

    // Fires once per match, not every cycle of the line.
    rig.clear_if();
    rig.ticks(100);
    assert_eq!(rig.ints.flags & STAT_BIT, 0);

    rig.run_to(6, 0);
    assert_eq!(rig.ppu.read_reg(0xFF41) & 0x04, 0);
}

#[test]
fn lyc_flag_is_set_without_interrupt_when_source_disabled() {
    let mut rig = Rig::at_line0();
    rig.write(0xFF45, 3);
    rig.clear_if();
    rig.run_to(3, 4);
    assert_eq!(rig.ppu.read_reg(0xFF41) & 0x04, 0x04);
    assert_eq!(rig.ints.flags & STAT_BIT, 0);
}

fn count_stat_interrupts_over_frame(select: u8) -> u32 {
    let mut rig = Rig::at_line0();
    rig.write(0xFF41, select);
    rig.clear_if();
    let mut n = 0;
    for _ in 0..M_CYCLES_PER_FRAME {
        rig.tick();
        if rig.ints.flags & STAT_BIT != 0 {
            n += 1;
            rig.clear_if();
        }
    }
    n
}

#[test]
fn stat_mode_sources_fire_on_their_mode_entries() {
    // Selecting a source while the line is already asserted (at_line0 is mode
    // 2) raises it immediately, so only the entries *after* line 0's are
    // counted for mode 2: lines 1..=143 plus the immediate one.
    assert_eq!(count_stat_interrupts_over_frame(0x08), 144); // HBlank: 144 lines
    assert_eq!(count_stat_interrupts_over_frame(0x10), 1); // VBlank: once
    // OAM: lines 0..=143, plus the pulse as line 144 (VBlank) begins.
    assert_eq!(count_stat_interrupts_over_frame(0x20), 145);
}

#[test]
fn stat_line_blocking_merges_adjacent_sources() {
    // Mode 0 of line N flows straight into mode 2 of line N+1 with both
    // selected, so the line never drops between them: only the mode 0 entry
    // of each visible line is a rising edge, plus the first mode 2 (line 0).
    assert_eq!(count_stat_interrupts_over_frame(0x28), 145);
}

#[test]
fn stat_write_with_asserted_source_raises_once() {
    let mut rig = Rig::at_line0();
    rig.run_to(10, 252); // mode 0
    assert_eq!(rig.ppu.mode(), 0);
    rig.clear_if();
    rig.write(0xFF41, 0x08);
    assert_ne!(rig.ints.flags & STAT_BIT, 0);
    rig.clear_if();
    rig.write(0xFF41, 0x08 | 0x40); // already high: blocked
    assert_eq!(rig.ints.flags & STAT_BIT, 0);
}

#[test]
fn lyc_irq_is_blocked_while_mode_source_holds_line() {
    let mut rig = Rig::at_line0();
    rig.write(0xFF41, 0x48); // HBlank + LYC
    rig.write(0xFF45, 20);
    rig.run_to(19, 300); // mode 0 of line 19: line high
    rig.clear_if();
    rig.run_to(20, 4); // line 20 mode 2, LYC match: line stayed high
    assert_eq!(rig.ints.flags & STAT_BIT, 0);
}

#[test]
fn stat_read_has_bit7_and_select_bits() {
    let mut rig = Rig::at_line0();
    rig.write(0xFF41, 0xFF);
    rig.tick(); // the mode bits lag the internal mode by one M-cycle
    let stat = rig.ppu.read_reg(0xFF41);
    assert_eq!(stat & 0xF8, 0xF8);
    assert_eq!(stat & 0x03, 2);
    // Mode / coincidence bits are read-only.
    rig.write(0xFF41, 0x00);
    assert_eq!(rig.ppu.read_reg(0xFF41) & 0x78, 0);
}

// ----------------------------------------------------------- LCD on / off

#[test]
fn lcd_off_resets_ly_mode_and_blanks_screen() {
    let mut rig = Rig::at_line0();
    rig.fill_map(0x1800, 1);
    rig.solid_tile(0x10, 3);
    let fb = rig.frame();
    assert!(fb.iter().all(|&p| p == 3));
    rig.ticks(500);
    assert_ne!(rig.ppu.ly, 0);

    rig.write(0xFF40, 0x11);
    assert_eq!(rig.ppu.read_reg(0xFF44), 0);
    assert_eq!(rig.ppu.read_reg(0xFF41) & 0x03, 0);
    assert!(rig.ppu.framebuffer().iter().all(|&p| p == 0));
    // CPU can reach VRAM/OAM with the LCD off.
    rig.ppu.write_vram(0x8000, 0x42);
    assert_eq!(rig.ppu.read_vram(0x8000), 0x42);
    rig.ppu.write_oam(0xFE00, 0x24);
    assert_eq!(rig.ppu.read_oam(0xFE00), 0x24);
}

#[test]
fn lcd_off_still_produces_a_frame_every_70224_dots() {
    let mut rig = Rig::at_line0();
    rig.write(0xFF40, 0x11);
    rig.clear_if();
    let mut mcycles = 0u32;
    let mut gaps = Vec::new();
    for _ in 0..(3 * M_CYCLES_PER_FRAME) {
        rig.tick();
        mcycles += 1;
        if rig.ppu.take_frame_ready() {
            gaps.push(mcycles * 4);
            mcycles = 0;
        }
    }
    assert_eq!(gaps, [70_224, 70_224, 70_224]);
    assert_eq!(rig.ints.flags & (STAT_BIT | VBLANK_BIT), 0);
    assert_eq!(rig.ppu.ly, 0);
}

#[test]
fn lcd_on_restarts_line_0_and_first_frame_is_blank() {
    let mut rig = Rig::at_line0();
    rig.fill_map(0x1800, 1);
    rig.solid_tile(0x10, 3);
    rig.write(0xFF40, 0x11);
    rig.ticks(1000);
    rig.write(0xFF40, 0x91);
    assert_eq!((rig.ppu.ly, rig.ppu.line_dot), (0, 4));
    // First line has no OAM scan: mode 0 for its first 80 dots.
    assert_eq!(rig.ppu.mode(), 0);
    rig.ticks(20);
    assert_eq!(rig.ppu.mode(), 3);

    let first = rig.frame();
    assert!(first.iter().all(|&p| p == 0), "first frame must stay blank");
    let second = rig.frame();
    assert!(second.iter().all(|&p| p == 3));
}

// -------------------------------------------------------------- BG render

#[test]
fn framebuffer_only_changes_at_vblank() {
    let mut rig = Rig::at_line0();
    rig.fill_map(0x1800, 1);
    rig.solid_tile(0x10, 3);
    rig.run_to(100, 0);
    assert!(rig.ppu.framebuffer().iter().all(|&p| p == 0));
    let fb = rig.frame();
    assert!(fb.iter().all(|&p| p == 3));
}

#[test]
fn bg_palette_maps_ids_to_shades() {
    let mut rig = Rig::at_line0();
    rig.set_tile(0x10, [[0, 1, 2, 3, 0, 1, 2, 3]; 8]);
    rig.fill_map(0x1800, 1);
    rig.write(0xFF47, 0b0001_1011); // id0->3, id1->2, id2->1, id3->0
    let fb = rig.frame();
    assert_eq!(&fb[..8], &[3, 2, 1, 0, 3, 2, 1, 0]);
}

#[test]
fn bg_scx_wraps_horizontally() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x10, 3);
    rig.ppu.vram[0x1800 + 31] = 1; // map column 31 is tile 1
    rig.write(0xFF43, 252);
    let fb = rig.frame();
    assert_eq!(&fb[..4], &[3, 3, 3, 3]);
    assert_eq!(pixel(&fb, 4, 0), 0);
    assert_eq!(pixel(&fb, 159, 0), 0);
}

#[test]
fn bg_scy_wraps_vertically() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x10, 3);
    rig.ppu.vram[0x1800 + 31 * 32] = 1; // map row 31, column 0
    rig.write(0xFF42, 248);
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 0), 3);
    assert_eq!(pixel(&fb, 7, 7), 3);
    assert_eq!(pixel(&fb, 0, 8), 0);
    assert_eq!(pixel(&fb, 8, 0), 0);
}

#[test]
fn bg_fine_scroll_shifts_pixels() {
    let mut rig = Rig::at_line0();
    rig.set_tile(0x10, [[3, 0, 0, 0, 0, 0, 0, 0]; 8]);
    rig.fill_map(0x1800, 1);
    rig.write(0xFF43, 3);
    let fb = rig.frame();
    // Tile column 0 pixel 0 is scrolled off; the next tile's pixel 0 is at x=5.
    assert_eq!(&fb[..8], &[0, 0, 0, 0, 0, 3, 0, 0]);
}

#[test]
fn tile_data_unsigned_and_signed_addressing() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x0000, 1); // 0x8000: tile 0 unsigned
    rig.solid_tile(0x1000, 2); // 0x9000: tile 0 signed
    rig.solid_tile(0x0800, 3); // 0x8800: tile 128 unsigned, tile -128 signed
    rig.fill_map(0x1800, 0);
    rig.ppu.vram[0x1800 + 1] = 0x80;

    rig.write(0xFF40, 0x91); // unsigned
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 0), 1);
    assert_eq!(pixel(&fb, 8, 0), 3);

    rig.write(0xFF40, 0x81); // signed
    rig.frame(); // partially old; take the next full frame
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 0), 2);
    assert_eq!(pixel(&fb, 8, 0), 3);
}

#[test]
fn bg_map_select_bit() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x10, 1);
    rig.solid_tile(0x20, 2);
    rig.fill_map(0x1800, 1);
    rig.fill_map(0x1C00, 2);
    rig.write(0xFF40, 0x91);
    rig.frame();
    assert_eq!(pixel(&rig.frame(), 0, 0), 1);
    rig.write(0xFF40, 0x99);
    rig.frame();
    assert_eq!(pixel(&rig.frame(), 0, 0), 2);
}

#[test]
fn lcdc_bit0_off_blanks_bg_but_keeps_sprites() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x10, 3);
    rig.fill_map(0x1800, 1);
    rig.solid_tile(0x20, 3);
    rig.set_sprite(0, 16, 8, 2, 0x80); // BG-priority sprite
    rig.write(0xFF40, 0x92); // BG off, OBJ on
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 20, 20), 0);
    // BG colour is 0 so even a BG-priority sprite shows.
    assert_eq!(pixel(&fb, 0, 0), 3);
}

// ----------------------------------------------------------------- window

#[test]
fn window_position_and_wx_offset() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x20, 3);
    rig.fill_map(0x1C00, 2);
    rig.write(0xFF40, 0xF1); // LCD, win map 9C00, win on, unsigned, BG on
    rig.write(0xFF4A, 10);
    rig.write(0xFF4B, 7 + 16);
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 100, 9), 0); // above WY
    assert_eq!(pixel(&fb, 15, 10), 0); // left of window
    assert_eq!(pixel(&fb, 16, 10), 3);
    assert_eq!(pixel(&fb, 159, 143), 3);
}

#[test]
fn window_wx_below_7_clips_left_columns() {
    let mut rig = Rig::at_line0();
    rig.set_tile(0x20, [[1, 2, 3, 1, 2, 3, 1, 2]; 8]);
    rig.fill_map(0x1C00, 2);
    rig.write(0xFF40, 0xF1);
    rig.write(0xFF4A, 0);
    rig.write(0xFF4B, 3); // window starts at x = -4
    let fb = rig.frame();
    assert_eq!(&fb[..4], &[2, 3, 1, 2]); // window columns 4..8
}

#[test]
fn window_hidden_when_wx_is_167_or_more() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x20, 3);
    rig.fill_map(0x1C00, 2);
    rig.write(0xFF40, 0xF1);
    rig.write(0xFF4A, 0);
    rig.write(0xFF4B, 167);
    let fb = rig.frame();
    assert!(fb.iter().all(|&p| p == 0));
}

#[test]
fn window_line_counter_only_advances_on_drawn_lines() {
    let mut rig = Rig::at_line0();
    // Window map rows 0, 1, 2 hold solid tiles of colour 1, 2, 3.
    rig.solid_tile(0x10, 1);
    rig.solid_tile(0x20, 2);
    rig.solid_tile(0x30, 3);
    for col in 0..32 {
        rig.ppu.vram[0x1C00 + col] = 1;
        rig.ppu.vram[0x1C00 + 32 + col] = 2;
        rig.ppu.vram[0x1C00 + 64 + col] = 3;
    }
    rig.write(0xFF40, 0xF1);
    rig.write(0xFF4A, 0);
    rig.write(0xFF4B, 7);
    // Window off for lines 8..16, back on for line 16.
    rig.run_to(8, 0);
    rig.write(0xFF40, 0xD1);
    rig.run_to(16, 0);
    rig.write(0xFF40, 0xF1);
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 7), 1); // window row 0
    assert_eq!(pixel(&fb, 0, 8), 0); // window disabled
    assert_eq!(pixel(&fb, 0, 15), 0);
    // Counter resumed at 8, i.e. window tile row 1 (colour 2), not row 2.
    assert_eq!(pixel(&fb, 0, 16), 2);
    assert_eq!(pixel(&fb, 0, 23), 2);
    assert_eq!(pixel(&fb, 0, 24), 3);
}

#[test]
fn window_counter_resets_each_frame() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x10, 1);
    rig.solid_tile(0x20, 2);
    for col in 0..32 {
        rig.ppu.vram[0x1C00 + col] = 1;
        rig.ppu.vram[0x1C00 + 32 + col] = 2;
    }
    rig.write(0xFF40, 0xF1);
    rig.write(0xFF4A, 0);
    rig.write(0xFF4B, 7);
    rig.frame();
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 0), 1);
    assert_eq!(pixel(&fb, 0, 8), 2);
}

// ---------------------------------------------------------------- sprites

/// Tile whose only opaque pixel is colour 3 at (col 0, row 0).
fn corner_tile() -> [[u8; 8]; 8] {
    let mut t = [[0u8; 8]; 8];
    t[0][0] = 3;
    t
}

#[test]
fn sprite_flips() {
    let mut rig = Rig::at_line0();
    rig.set_tile(0x20, corner_tile());
    rig.write(0xFF40, 0x93);
    rig.set_sprite(0, 16, 8, 2, 0x00);
    rig.set_sprite(1, 16, 24, 2, 0x20); // x flip
    rig.set_sprite(2, 32, 8, 2, 0x40); // y flip
    rig.set_sprite(3, 32, 24, 2, 0x60); // both
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 0), 3);
    assert_eq!(pixel(&fb, 7, 0), 0);
    assert_eq!(pixel(&fb, 16 + 7, 0), 3);
    assert_eq!(pixel(&fb, 16, 0), 0);
    assert_eq!(pixel(&fb, 0, 16 + 7), 3);
    assert_eq!(pixel(&fb, 0, 16), 0);
    assert_eq!(pixel(&fb, 16 + 7, 16 + 7), 3);
}

#[test]
fn sprite_8x16_uses_tile_pair_and_ignores_bit0() {
    let mut rig = Rig::at_line0();
    rig.set_tile(0x20, corner_tile()); // tile 2: top half
    let mut lower = [[0u8; 8]; 8];
    lower[7][7] = 2;
    rig.set_tile(0x30, lower); // tile 3: bottom half
    rig.write(0xFF40, 0x97);
    rig.set_sprite(0, 16, 8, 3, 0x00); // tile index 3 -> pair 2/3
    rig.set_sprite(1, 16, 40, 2, 0x40); // y flip, bit0 clear
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 0), 3); // tile 2 row 0
    assert_eq!(pixel(&fb, 7, 15), 2); // tile 3 row 7
    // Y flip swaps the halves and flips each: bottom-half pixel lands on top.
    assert_eq!(pixel(&fb, 32 + 7, 0), 2);
    assert_eq!(pixel(&fb, 32, 15), 3);
}

#[test]
fn sprite_palettes_obp0_obp1() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x20, 1);
    rig.write(0xFF40, 0x93);
    rig.write(0xFF48, 0b1110_0100); // id1 -> 1
    rig.write(0xFF49, 0b1110_1100); // id1 -> 3
    rig.set_sprite(0, 16, 8, 2, 0x00);
    rig.set_sprite(1, 16, 24, 2, 0x10);
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 0), 1);
    assert_eq!(pixel(&fb, 16, 0), 3);
}

#[test]
fn sprite_color0_is_transparent_regardless_of_palette() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x10, 2);
    rig.fill_map(0x1800, 1);
    rig.write(0xFF40, 0x93);
    rig.write(0xFF48, 0xFF); // id0 would map to black
    rig.set_sprite(0, 16, 8, 5, 0x00); // tile 5 is empty
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 0), 2);
}

#[test]
fn sprites_do_not_draw_when_obj_disabled() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x20, 3);
    rig.set_sprite(0, 16, 8, 2, 0);
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 0), 0);
}

#[test]
fn only_ten_sprites_per_line_in_oam_order() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x20, 3);
    rig.write(0xFF40, 0x93);
    for i in 0..10 {
        rig.set_sprite(i, 16, 16 + 8 * i as u8, 2, 0);
    }
    // 11th by OAM order, but lowest X: must be dropped, not preferred.
    rig.set_sprite(10, 16, 8, 2, 0);
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 0), 0);
    for i in 0..10 {
        assert_eq!(pixel(&fb, 8 + 8 * i, 0), 3, "sprite {i}");
    }
    // The limit is per line: the same sprite is fine on another line.
    rig.set_sprite(10, 16 + 40, 8, 2, 0);
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 40), 3);
}

#[test]
fn offscreen_sprites_still_count_toward_the_limit() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x20, 3);
    rig.write(0xFF40, 0x93);
    for i in 0..5 {
        rig.set_sprite(i, 16, 0, 2, 0); // X = 0: invisible
    }
    for i in 5..10 {
        rig.set_sprite(i, 16, 168, 2, 0); // X >= 168: invisible
    }
    rig.set_sprite(10, 16, 8, 2, 0);
    rig.run_to(1, 84);
    assert_eq!(rig.ppu.line_sprite_count, 10);
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 0), 0);
}

#[test]
fn partially_offscreen_sprites_are_clipped() {
    let mut rig = Rig::at_line0();
    rig.set_tile(0x20, [[3; 8]; 8]);
    rig.write(0xFF40, 0x93);
    rig.set_sprite(0, 16, 4, 2, 0); // columns 4..8 visible at x 0..4
    rig.set_sprite(1, 32, 164, 2, 0); // only x 156..160 visible
    let fb = rig.frame();
    assert_eq!(&fb[..5], &[3, 3, 3, 3, 0]);
    assert_eq!(pixel(&fb, 155, 16), 0);
    assert_eq!(pixel(&fb, 156, 16), 3);
    assert_eq!(pixel(&fb, 159, 16), 3);
}

#[test]
fn lower_x_sprite_wins_overlaps_and_ties_go_to_lower_oam_index() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x20, 3);
    rig.solid_tile(0x30, 1);
    rig.write(0xFF40, 0x93);
    // A: OAM 0, screen x 8..16, colour 3 (shade 3). B: OAM 1, screen x 4..12,
    // colour 1 (OBP1 -> shade 2). B has the lower X so it wins 8..12.
    rig.set_sprite(0, 16, 16, 2, 0x00);
    rig.set_sprite(1, 16, 12, 3, 0x10);
    // Tie: C (OAM 2) vs D (OAM 3) at the same X; C wins.
    rig.set_sprite(2, 32, 16, 3, 0x00); // colour 1 -> shade 1 (OBP0)
    rig.set_sprite(3, 32, 16, 2, 0x00); // colour 3 -> shade 3
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 6, 0), 2);
    assert_eq!(pixel(&fb, 10, 0), 2);
    assert_eq!(pixel(&fb, 12, 0), 3);
    assert_eq!(pixel(&fb, 10, 16), 1);
}

#[test]
fn lower_x_wins_even_with_higher_oam_index_and_transparent_pixels_fall_through() {
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x20, 3);
    let mut half = [[0u8; 8]; 8];
    for row in &mut half {
        row[4..].fill(1);
    }
    rig.set_tile(0x30, half);
    rig.write(0xFF40, 0x93);
    rig.set_sprite(0, 16, 16, 2, 0x00); // A: x 8..16 colour 3, OAM 0
    rig.set_sprite(1, 16, 16, 3, 0x00); // B: same X, OAM 1 -> A wins ties
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 12, 0), 3);

    // Swap roles: B (half transparent) gets the lower OAM index.
    rig.set_sprite(0, 16, 16, 3, 0x00);
    rig.set_sprite(1, 16, 16, 2, 0x00);
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 8, 0), 3); // B transparent: A shows through
    assert_eq!(pixel(&fb, 12, 0), 1); // B opaque (colour 1 -> shade 1)
}

#[test]
fn bg_over_obj_flag_only_yields_to_bg_colors_1_to_3() {
    let mut rig = Rig::at_line0();
    rig.set_tile(0x10, [[2, 2, 2, 2, 0, 0, 0, 0]; 8]);
    rig.fill_map(0x1800, 1);
    rig.solid_tile(0x20, 3);
    rig.write(0xFF40, 0x93);
    rig.write(0xFF47, 0b1110_0101); // BG id0 -> shade 1 (non-white), id2 -> 2
    rig.set_sprite(0, 16, 8, 2, 0x80); // BG priority sprite at x 0..8
    rig.set_sprite(1, 32, 8, 2, 0x00); // normal sprite for comparison
    let fb = rig.frame();
    assert_eq!(&fb[..8], &[2, 2, 2, 2, 3, 3, 3, 3]);
    assert_eq!(&fb[16 * SCREEN_WIDTH..16 * SCREEN_WIDTH + 8], &[3; 8]);
}

#[test]
fn bg_priority_sprite_does_not_hide_a_lower_priority_sprite() {
    // The highest-priority *opaque* sprite pixel decides; if it yields to BG
    // the BG shows, not the sprite beneath it.
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x10, 1);
    rig.fill_map(0x1800, 1);
    rig.solid_tile(0x20, 3);
    rig.write(0xFF40, 0x93);
    rig.set_sprite(0, 16, 8, 2, 0x80);
    rig.set_sprite(1, 16, 8, 2, 0x00);
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 0), 1);
}

// ---------------------------------------------------- CPU access blocking

#[test]
fn vram_blocked_in_mode_3_only() {
    let mut rig = Rig::at_line0();
    rig.ppu.vram[0x100] = 0x5A;
    // (dot, read blocked, write blocked): reads lock a cycle before writes.
    for (dot, read_blocked, write_blocked) in [
        (0u16, false, false),
        (76, false, false),
        (80, true, false),
        (84, true, true),
        (248, true, true),
        (252, true, true),
        (256, false, false),
    ] {
        rig.run_to(3, dot);
        assert_eq!(
            rig.ppu.read_vram(0x8100),
            if read_blocked { 0xFF } else { 0x5A },
            "dot {dot}"
        );
        rig.ppu.write_vram(0x8100, 0x11);
        let written = rig.ppu.vram[0x100] == 0x11;
        assert_eq!(written, !write_blocked, "dot {dot}");
        rig.ppu.vram[0x100] = 0x5A;
    }
}

#[test]
fn oam_blocked_in_modes_2_and_3() {
    let mut rig = Rig::at_line0();
    rig.ppu.oam[0] = 0x77;
    // (line, dot, read blocked, write blocked). Reads lock from the first
    // cycle of the line; writes follow the mode bits, with a gap at dot 80.
    let cases = [
        (5u8, 0u16, true, false),
        (5, 4, true, true),
        (5, 76, true, true),
        (5, 80, true, false),
        (5, 84, true, true),
        (5, 252, true, true),
        (5, 256, false, false),
        (150, 100, false, false),
    ];
    for (ly, dot, read_blocked, write_blocked) in cases {
        rig.run_to(ly, dot);
        assert_eq!(
            rig.ppu.read_oam(0xFE00),
            if read_blocked { 0xFF } else { 0x77 },
            "{ly}/{dot}"
        );
        rig.ppu.write_oam(0xFE00, 0x11);
        assert_eq!(rig.ppu.oam[0] == 0x11, !write_blocked, "{ly}/{dot}");
        rig.ppu.oam[0] = 0x77;
    }
}

#[test]
fn oam_dma_write_bypasses_blocking() {
    let mut rig = Rig::at_line0();
    rig.run_to(5, 100); // mode 3
    rig.ppu.oam_dma_write(3, 0xAB);
    assert_eq!(rig.ppu.oam[3], 0xAB);
    assert_eq!(rig.ppu.read_oam(0xFE03), 0xFF);
}

#[test]
fn blocked_oam_does_not_stop_sprites_from_the_scan() {
    // OAM contents (written via DMA) are used by the scan irrespective of CPU
    // access rules.
    let mut rig = Rig::at_line0();
    rig.solid_tile(0x20, 3);
    rig.write(0xFF40, 0x93);
    for (i, b) in [16u8, 8, 2, 0].into_iter().enumerate() {
        rig.ppu.oam_dma_write(i as u8, b);
    }
    let fb = rig.frame();
    assert_eq!(pixel(&fb, 0, 0), 3);
}

// ------------------------------------------------ LCD on/off and VBlank STAT

#[test]
fn lyc_flag_is_retained_while_lcd_is_off() {
    let mut rig = Rig::at_line0();
    rig.write(0xFF45, 0x90);
    rig.run_to(0x90, 4);
    assert_eq!(rig.ppu.read_reg(0xFF41) & 0x04, 0x04);
    rig.write(0xFF40, 0x11);
    assert_eq!(rig.ppu.read_reg(0xFF41) & 0x04, 0x04);
    // The comparison clock is stopped: LYC writes do not touch the flag.
    rig.write(0xFF45, 0x01);
    assert_eq!(rig.ppu.read_reg(0xFF41) & 0x04, 0x04);
    // Turning the LCD on compares LY = 0 with the new LYC straight away.
    rig.write(0xFF40, 0x91);
    assert_eq!(rig.ppu.read_reg(0xFF41) & 0x04, 0);
}

#[test]
fn lyc_irq_is_not_retriggered_by_lcd_on_when_flag_stays_set() {
    let mut rig = Rig::at_line0();
    rig.write(0xFF41, 0x40);
    rig.write(0xFF45, 0x90);
    rig.run_to(0x90, 0);
    rig.write(0xFF40, 0x11);
    rig.write(0xFF45, 0x00);
    rig.clear_if();
    rig.write(0xFF40, 0x91);
    assert_eq!(rig.ints.flags & STAT_BIT, 0, "flag 1 -> 1 is not an edge");
}

#[test]
fn lyc_irq_fires_when_lcd_on_sets_the_flag() {
    let mut rig = Rig::at_line0();
    rig.write(0xFF41, 0x40);
    rig.write(0xFF45, 0x00);
    rig.run_to(0x90, 0);
    rig.write(0xFF40, 0x11);
    rig.clear_if();
    rig.write(0xFF40, 0x91);
    assert_eq!(rig.ints.flags & STAT_BIT, STAT_BIT);
}

#[test]
fn oam_source_pulses_with_vblank_on_line_144() {
    let mut rig = Rig::at_line0();
    rig.write(0xFF41, STAT_OAM_IRQ);
    rig.run_to(143, 452);
    rig.clear_if();
    rig.tick();
    assert_eq!(rig.ints.flags & VBLANK_BIT, VBLANK_BIT);
    assert_eq!(rig.ints.flags & STAT_BIT, STAT_BIT);
}

/// Reads taken after an LCD-on write, as in mooneye `lcdon_timing-GS`: each
/// pass delays the first read by 0/1/2 extra M-cycles, `READ_CYCLES` are the
/// M-cycle gaps of the eight reads, and the tables list the expected value of
/// each read for the three passes.
const READ_CYCLES: [u32; 8] = [0, 17, 60, 110, 130, 174, 224, 244];

fn lcd_on_read(pass: u32, cycles: u32, read: impl Fn(&Ppu) -> u8, lyc: u8) -> u8 {
    let mut rig = Rig::new();
    rig.write(0xFF40, 0x01);
    rig.write(0xFF45, lyc);
    rig.write(0xFF40, 0x81);
    // The write's own M-cycle is already spent; the read is 2 cycles later.
    rig.ticks(2 + pass + cycles);
    read(&rig.ppu)
}

fn check_lcd_on_table(table: [[u8; 8]; 3], lyc: u8, read: impl Fn(&Ppu) -> u8 + Copy) {
    for (pass, row) in table.iter().enumerate() {
        for (&cycles, &want) in READ_CYCLES.iter().zip(row) {
            let got = lcd_on_read(pass as u32, cycles, read, lyc);
            assert_eq!(got, want, "pass {pass} cycle {}", cycles + pass as u32);
        }
    }
}

#[test]
fn lcd_on_ly_timeline() {
    check_lcd_on_table(
        [
            [0, 0, 0, 0, 1, 1, 1, 2],
            [0, 0, 0, 1, 1, 1, 2, 2],
            [0, 0, 0, 1, 1, 1, 2, 2],
        ],
        0,
        |p| p.read_reg(0xFF44),
    );
}

#[test]
fn lcd_on_stat_timeline() {
    let stat = |p: &Ppu| p.read_reg(0xFF41);
    check_lcd_on_table(
        [
            [0x84, 0x84, 0x87, 0x84, 0x82, 0x83, 0x80, 0x82],
            [0x84, 0x87, 0x84, 0x80, 0x82, 0x80, 0x80, 0x82],
            [0x84, 0x87, 0x84, 0x82, 0x83, 0x80, 0x82, 0x83],
        ],
        0,
        stat,
    );
    check_lcd_on_table(
        [
            [0x80, 0x80, 0x83, 0x80, 0x86, 0x87, 0x84, 0x82],
            [0x80, 0x83, 0x80, 0x80, 0x86, 0x84, 0x80, 0x82],
            [0x80, 0x83, 0x80, 0x86, 0x87, 0x84, 0x82, 0x83],
        ],
        1,
        stat,
    );
}

#[test]
fn lcd_on_oam_and_vram_read_timeline() {
    check_lcd_on_table(
        [
            [0x00, 0x00, 0xFF, 0x00, 0xFF, 0xFF, 0x00, 0xFF],
            [0x00, 0xFF, 0x00, 0xFF, 0xFF, 0x00, 0xFF, 0xFF],
            [0x00, 0xFF, 0x00, 0xFF, 0xFF, 0x00, 0xFF, 0xFF],
        ],
        0,
        |p| p.read_oam(0xFE00),
    );
    check_lcd_on_table(
        [
            [0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00],
            [0x00, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF],
            [0x00, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF],
        ],
        0,
        |p| p.read_vram(0x8000),
    );
}
