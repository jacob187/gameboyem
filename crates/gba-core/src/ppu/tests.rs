//! PPU unit tests: the `Ppu` is driven directly through the scheduler with
//! hand-built VRAM / palette / OAM / register state, no CPU or cartridge.

use super::*;
use crate::{CYCLES_PER_FRAME, SCREEN_WIDTH, VISIBLE_LINES};

mod affine_tests;
mod bg_tests;
mod obj_tests;
mod window_tests;

pub(super) const DISPCNT: u32 = 0x000;
pub(super) const DISPSTAT: u32 = 0x004;
pub(super) const VCOUNT: u32 = 0x006;

/// A PPU plus its scheduler, collecting the signals of every event.
pub(super) struct Rig {
    pub ppu: Ppu,
    pub sched: Scheduler,
    pub irq: u16,
    pub hblank_dmas: u32,
    pub vblank_dmas: u32,
    pub frames: u32,
}

impl Rig {
    pub fn new() -> Self {
        let mut ppu = Ppu::new();
        let mut sched = Scheduler::new();
        ppu.start(&mut sched);
        Self {
            ppu,
            sched,
            irq: 0,
            hblank_dmas: 0,
            vblank_dmas: 0,
            frames: 0,
        }
    }

    /// Run the clock to exactly `cycle`, dispatching every PPU event due on the way.
    pub fn run_to(&mut self, cycle: u64) {
        assert!(cycle >= self.sched.now());
        while let Some(at) = self.sched.next_at() {
            if at > cycle {
                break;
            }
            self.sched.skip_to_next();
            while let Some((ev, at)) = self.sched.pop_due() {
                let sig = self.ppu.on_event(ev, at, &mut self.sched);
                self.irq |= sig.irq;
                self.hblank_dmas += u32::from(sig.hblank_dma);
                self.vblank_dmas += u32::from(sig.vblank_dma);
                self.frames += u32::from(sig.frame_done);
            }
        }
        self.sched.advance(cycle - self.sched.now());
    }

    /// Run to the start of `line` of the current frame (cycle `line * 1232`) plus `dot_cycles`.
    pub fn run_to_line(&mut self, line: u64, cycle_in_line: u64) {
        let frame_base =
            self.sched.now() / u64::from(CYCLES_PER_FRAME) * u64::from(CYCLES_PER_FRAME);
        self.run_to(frame_base + line * u64::from(CYCLES_PER_LINE) + cycle_in_line);
    }

    pub fn write(&mut self, off: u32, v: u16) {
        self.ppu.write_io(off, v, 0xFFFF);
    }

    pub fn stat(&self) -> u16 {
        self.ppu.read_io(DISPSTAT)
    }

    /// Run until the next frame completes and return it.
    pub fn frame(&mut self) -> Vec<u16> {
        let target = self.frames + 1;
        while self.frames < target {
            let next = self.sched.next_at().expect("PPU events are always pending");
            self.run_to(next);
        }
        self.ppu.framebuffer().to_vec()
    }

    pub fn vram16(&mut self, addr: usize, v: u16) {
        self.ppu.vram[addr..addr + 2].copy_from_slice(&v.to_le_bytes());
    }

    /// Palette RAM entry `i` (0..256 BG, 256..512 OBJ).
    pub fn pal(&mut self, i: usize, colour: u16) {
        self.ppu.palette[i * 2..i * 2 + 2].copy_from_slice(&colour.to_le_bytes());
    }

    /// Give every palette entry the distinct colour `i * 7 + 1` (entry 0, the
    /// backdrop, is 1), see [`pal_colour`].
    pub fn distinct_palette(&mut self) {
        for i in 0..512 {
            self.pal(i, pal_colour(i));
        }
    }

    pub fn oam(&mut self, index: usize, attr0: u16, attr1: u16, attr2: u16) {
        let o = index * 8;
        self.ppu.oam[o..o + 2].copy_from_slice(&attr0.to_le_bytes());
        self.ppu.oam[o + 2..o + 4].copy_from_slice(&attr1.to_le_bytes());
        self.ppu.oam[o + 4..o + 6].copy_from_slice(&attr2.to_le_bytes());
    }

    /// Park every sprite off screen and disabled (OAM resets to zero, which is a
    /// visible 8x8 sprite at (0,0)).
    pub fn hide_all_sprites(&mut self) {
        for i in 0..128 {
            self.oam(i, 0x0200, 0, 0);
        }
    }
}

/// DISPCNT enable bit of BG `bg`.
pub(super) fn bg_on(bg: usize) -> u16 {
    0x100 << bg
}

/// Write a 4bpp tile whose pixel (r, c) is `1 + (r * 3 + c) % 15`.
pub(super) fn tile4(rig: &mut Rig, char_base: usize, tile: usize) {
    for r in 0..8 {
        for c in 0..4 {
            let lo = 1 + (r * 3 + 2 * c) % 15;
            let hi = 1 + (r * 3 + 2 * c + 1) % 15;
            rig.ppu.vram[char_base + tile * 32 + r * 4 + c] = (lo | hi << 4) as u8;
        }
    }
}

/// The 4bpp test tile's palette index at row `r`, column `c`.
pub(super) fn t4(r: usize, c: usize) -> usize {
    1 + (r * 3 + c) % 15
}

/// A BG whose every cell is a solid 8bpp tile of palette index `idx` (char base 0,
/// tile `1 + bg`, screen block `8 + bg`).
pub(super) fn solid_bg(rig: &mut Rig, bg: usize, idx: u8, priority: u16) {
    let tile = 1 + bg;
    rig.ppu.vram[tile * 64..tile * 64 + 64].fill(idx);
    let screen_base = (8 + bg) * 0x800;
    for i in 0..32 * 32 {
        rig.vram16(screen_base + i * 2, tile as u16);
    }
    let cnt = priority | 1 << 7 | ((8 + bg) as u16) << 8;
    rig.write(0x008 + 2 * bg as u32, cnt);
}

/// The colour [`Rig::distinct_palette`] gives entry `i`.
pub(super) fn pal_colour(i: usize) -> u16 {
    (i as u16) * 7 + 1
}

/// Pixel (x, y) of a frame.
pub(super) fn px(frame: &[u16], x: usize, y: usize) -> u16 {
    frame[y * SCREEN_WIDTH + x]
}

/// 15-bit colour from 5-bit components.
pub(super) fn rgb(r: u16, g: u16, b: u16) -> u16 {
    r | g << 5 | b << 10
}

#[test]
fn a_frame_is_228_lines_and_signals_vblank_once() {
    let mut rig = Rig::new();
    let mut lines = 0;
    while rig.sched.now() < u64::from(CYCLES_PER_FRAME) {
        rig.sched.skip_to_next();
        while let Some((ev, at)) = rig.sched.pop_due() {
            let sig = rig.ppu.on_event(ev, at, &mut rig.sched);
            rig.frames += u32::from(sig.frame_done);
            lines += u32::from(ev == Event::PpuLineEnd);
        }
    }
    assert_eq!(lines, LINES_PER_FRAME);
    assert_eq!(rig.frames, 1);
    assert_eq!(rig.ppu.vcount(), 0);
}

#[test]
fn vcount_advances_at_each_line_end_and_is_read_only() {
    let mut rig = Rig::new();
    rig.run_to_line(0, 1231);
    assert_eq!(rig.ppu.read_io(VCOUNT), 0);
    rig.run_to_line(1, 0);
    assert_eq!(rig.ppu.read_io(VCOUNT), 1);
    rig.write(VCOUNT, 0x55);
    assert_eq!(rig.ppu.read_io(VCOUNT), 1);
    rig.run_to_line(227, 1231);
    assert_eq!(rig.ppu.read_io(VCOUNT), 227);
    rig.run_to(u64::from(CYCLES_PER_FRAME));
    assert_eq!(rig.ppu.read_io(VCOUNT), 0);
}

#[test]
fn hblank_flag_rises_at_cycle_1006_and_clears_at_line_end() {
    let mut rig = Rig::new();
    rig.run_to_line(5, 1005);
    assert_eq!(rig.stat() & 2, 0);
    rig.run_to_line(5, 1006);
    assert_eq!(rig.stat() & 2, 2);
    rig.run_to_line(5, 1231);
    assert_eq!(rig.stat() & 2, 2);
    rig.run_to_line(6, 0);
    assert_eq!(rig.stat() & 2, 0);
    // It toggles in the VBlank lines too.
    rig.run_to_line(200, 1100);
    assert_eq!(rig.stat() & 2, 2);
}

#[test]
fn vblank_flag_covers_lines_160_to_226() {
    let mut rig = Rig::new();
    rig.run_to_line(159, 1231);
    assert_eq!(rig.stat() & 1, 0);
    rig.run_to_line(160, 0);
    assert_eq!(rig.stat() & 1, 1);
    rig.run_to_line(226, 1231);
    assert_eq!(rig.stat() & 1, 1);
    rig.run_to_line(227, 0);
    assert_eq!(rig.stat() & 1, 0);
}

#[test]
fn vcount_match_flag_follows_lyc() {
    let mut rig = Rig::new();
    rig.write(DISPSTAT, 7 << 8);
    rig.run_to_line(6, 1231);
    assert_eq!(rig.stat() & 4, 0);
    rig.run_to_line(7, 0);
    assert_eq!(rig.stat() & 4, 4);
    rig.run_to_line(8, 0);
    assert_eq!(rig.stat() & 4, 0);
    // LYC beyond the last line never matches.
    rig.write(DISPSTAT, 250 << 8);
    rig.run_to_line(227, 0);
    assert_eq!(rig.stat() & 4, 0);
}

#[test]
fn dispstat_write_mask_keeps_status_bits_read_only() {
    let mut rig = Rig::new();
    rig.write(DISPSTAT, 0xFFFF);
    // Bits 0-2 are status (all clear on line 0 with LYC 0xFF), bits 6-7 are unused,
    // bits 3-5 and 8-15 stick.
    assert_eq!(rig.stat(), 0xFF38);
    // Byte-lane writes: the low byte alone leaves LYC alone.
    rig.ppu.write_io(DISPSTAT, 0x0000, 0x00FF);
    assert_eq!(rig.stat(), 0xFF00);
    rig.ppu.write_io(DISPSTAT, 0x1200, 0xFF00);
    assert_eq!(rig.stat(), 0x1200);
}

#[test]
fn irq_signals_are_gated_by_the_dispstat_enables() {
    // Nothing enabled: no IRQ over a whole frame.
    let mut rig = Rig::new();
    rig.run_to(u64::from(CYCLES_PER_FRAME));
    assert_eq!(rig.irq, 0);

    // VBlank only: bit 0 at line 160.
    let mut rig = Rig::new();
    rig.write(DISPSTAT, 1 << 3);
    rig.run_to_line(159, 1231);
    assert_eq!(rig.irq, 0);
    rig.run_to_line(160, 0);
    assert_eq!(rig.irq, 1);

    // HBlank only: bit 1 at cycle 1006 of line 0, including in VBlank lines.
    let mut rig = Rig::new();
    rig.write(DISPSTAT, 1 << 4);
    rig.run_to_line(0, 1005);
    assert_eq!(rig.irq, 0);
    rig.run_to_line(0, 1006);
    assert_eq!(rig.irq, 2);
    let mut rig = Rig::new();
    rig.write(DISPSTAT, 1 << 4);
    rig.run_to_line(200, 1000);
    rig.irq = 0;
    rig.run_to_line(200, 1006);
    assert_eq!(rig.irq, 2);

    // VCount only: bit 2 as line 20 starts, once per frame.
    let mut rig = Rig::new();
    rig.write(DISPSTAT, 1 << 5 | 20 << 8);
    rig.run_to_line(19, 1231);
    assert_eq!(rig.irq, 0);
    rig.run_to_line(20, 0);
    assert_eq!(rig.irq, 4);
}

#[test]
fn vcount_irq_fires_only_at_line_start_not_on_write() {
    let mut rig = Rig::new();
    rig.run_to_line(30, 100);
    rig.write(DISPSTAT, 1 << 5 | 30 << 8);
    rig.run_to_line(31, 0);
    assert_eq!(rig.irq, 0);
    assert_eq!(rig.stat() & 4, 0);
}

#[test]
fn dma_triggers_only_in_the_right_lines() {
    let mut rig = Rig::new();
    rig.run_to_line(160, 0);
    // HBlank DMA on lines 0..=159 only; VBlank DMA at line 160.
    assert_eq!(rig.hblank_dmas, VISIBLE_LINES);
    assert_eq!(rig.vblank_dmas, 1);
    rig.run_to_line(227, 1231);
    assert_eq!(rig.hblank_dmas, VISIBLE_LINES);
    assert_eq!(rig.vblank_dmas, 1);
}

#[test]
fn io_register_read_masks() {
    let mut rig = Rig::new();
    rig.write(0x008, 0xFFFF);
    rig.write(0x00C, 0xFFFF);
    assert_eq!(rig.ppu.read_io(0x008), 0xDFFF);
    assert_eq!(rig.ppu.read_io(0x00C), 0xFFFF);
    // Scroll registers are write-only.
    rig.write(0x010, 0x1FF);
    assert_eq!(rig.ppu.read_io(0x010), 0);
    rig.write(0x050, 0xFFFF);
    assert_eq!(rig.ppu.read_io(0x050), 0x3FFF);
    rig.write(DISPCNT, 0x1234);
    assert_eq!(rig.ppu.dispcnt(), 0x1234);
    assert_eq!(rig.ppu.read_io(DISPCNT), 0x1234);
}
