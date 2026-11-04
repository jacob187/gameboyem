//! Pixel processing unit (Pan Docs: "Rendering", "LCD Control", "LCD Status",
//! "Scrolling", "Palettes", "OAM", "STAT" / "STAT interrupt").
//!
//! # Contract (frozen; propose changes to the orchestrator)
//!
//! * [`Ppu::new`] creates the PPU in the DMG post-boot state.
//! * [`Ppu::tick_mcycle`] advances 4 dots and requests VBlank / STAT interrupts.
//! * [`Ppu::take_frame_ready`] returns true once per frame: when LY reaches 144
//!   with the LCD on, and every 70224 dots while the LCD is off (so
//!   `GameBoy::step_frame` always terminates).
//! * [`Ppu::framebuffer`] is the last *completed* frame: 160x144 shades 0..=3
//!   after BGP/OBP mapping. The LCD-off frame is blank (shade 0).
//! * [`Ppu::read_vram`] / [`Ppu::write_vram`] / [`Ppu::read_oam`] /
//!   [`Ppu::write_oam`] apply mode blocking; [`Ppu::oam_dma_write`] does not.
//! * [`Ppu::read_reg`] / [`Ppu::write_reg`] cover 0xFF40..=0xFF4B except DMA
//!   (0xFF46, owned by the DMA unit). Writes take `&mut Interrupts` because a
//!   STAT/LYC/LCDC write can raise the STAT line.
//!
//! # Model
//!
//! Timing is dot-exact at the line level: every line is 456 dots; visible lines
//! are mode 2 (80 dots), mode 3 (172 + extension dots) then mode 0. Lines
//! 144..=153 are mode 1. The whole scanline is rendered in one go at the mode 2
//! -> mode 3 transition (see `render`); mid-line register changes therefore
//! apply from the next line. Mode 3's length (and so the mode 0 start) is
//! extended by `SCX % 8`, the window and sprites using an approximation of the
//! penalties documented in Pan Docs ("Mode 3 length").
//!
//! Rendering goes to a back buffer that is swapped to the front on entering
//! VBlank, so [`Ppu::framebuffer`] never shows a half-drawn frame.

mod render;
#[cfg(test)]
mod tests;

use crate::interrupts::{Interrupt, Interrupts};
use crate::{Framebuffer, SCREEN_HEIGHT, SCREEN_WIDTH};

const DOTS_PER_LINE: u16 = 456;
const LINES_PER_FRAME: u8 = 154;
const DOTS_PER_FRAME: u32 = crate::T_CYCLES_PER_FRAME;
const VBLANK_LINE: u8 = SCREEN_HEIGHT as u8;
const OAM_SCAN_DOTS: u16 = 80;
const MODE3_BASE_DOTS: u16 = 172;

const LCDC_BG_ENABLE: u8 = 0x01;
const LCDC_OBJ_ENABLE: u8 = 0x02;
const LCDC_OBJ_TALL: u8 = 0x04;
const LCDC_BG_MAP: u8 = 0x08;
const LCDC_TILE_DATA: u8 = 0x10;
const LCDC_WIN_ENABLE: u8 = 0x20;
const LCDC_WIN_MAP: u8 = 0x40;
const LCDC_LCD_ENABLE: u8 = 0x80;

const STAT_HBLANK_IRQ: u8 = 0x08;
const STAT_VBLANK_IRQ: u8 = 0x10;
const STAT_OAM_IRQ: u8 = 0x20;
const STAT_LYC_IRQ: u8 = 0x40;
const STAT_WRITABLE: u8 = 0x78;

pub struct Ppu {
    vram: Box<[u8; 0x2000]>,
    oam: [u8; 0xA0],
    lcdc: u8,
    /// STAT bits 3..=6 (interrupt select). Mode / coincidence are derived.
    stat_select: u8,
    scy: u8,
    scx: u8,
    ly: u8,
    lyc: u8,
    bgp: u8,
    obp0: u8,
    obp1: u8,
    wy: u8,
    wx: u8,
    /// Dot within the current line, 0..456.
    line_dot: u16,
    /// Dots elapsed while the LCD is off (for the LCD-off frame cadence).
    off_dots: u32,
    /// Length of this line's mode 3 in dots (set when mode 3 begins).
    mode3_len: u16,
    /// True on the first line after the LCD is switched on: no OAM scan, and
    /// the PPU reports mode 0 instead of 2 for its first 80 dots.
    first_line: bool,
    /// The first frame after the LCD is switched on is not shown.
    skip_frame: bool,
    /// Latched when LY == WY has been seen this frame (window may start).
    wy_triggered: bool,
    /// Internal window line counter; only advances on lines that drew the window.
    win_line: u8,
    /// OAM indices of the (up to 10) sprites selected for the current line,
    /// sorted by drawing priority (lowest X first, ties by OAM index).
    line_sprites: [u8; 10],
    line_sprite_count: u8,
    /// Previous level of the STAT interrupt line (for rising-edge detection).
    stat_line: bool,
    back: Box<Framebuffer>,
    front: Box<Framebuffer>,
    frame_ready: bool,
}

impl Default for Ppu {
    fn default() -> Self {
        Self::new()
    }
}

impl Ppu {
    /// DMG post-boot state: LCD on, STAT reads 0x85 (mode 1, LY == LYC == 0).
    ///
    /// That STAT value is only reachable in the LY=153 quirk window (LY already
    /// reads 0 while the PPU is still in VBlank), so power-on starts there.
    pub fn new() -> Self {
        Self {
            vram: Box::new([0; 0x2000]),
            oam: [0; 0xA0],
            lcdc: 0x91,
            stat_select: 0x00,
            scy: 0,
            scx: 0,
            ly: LINES_PER_FRAME - 1,
            lyc: 0,
            bgp: 0xFC,
            obp0: 0xFF,
            obp1: 0xFF,
            wy: 0,
            wx: 0,
            line_dot: 8,
            off_dots: 0,
            mode3_len: MODE3_BASE_DOTS,
            first_line: false,
            skip_frame: false,
            wy_triggered: false,
            win_line: 0,
            line_sprites: [0; 10],
            line_sprite_count: 0,
            stat_line: false,
            back: Box::new([0; SCREEN_WIDTH * SCREEN_HEIGHT]),
            front: Box::new([0; SCREEN_WIDTH * SCREEN_HEIGHT]),
            frame_ready: false,
        }
    }

    fn lcd_on(&self) -> bool {
        self.lcdc & LCDC_LCD_ENABLE != 0
    }

    /// Current STAT mode (0 HBlank, 1 VBlank, 2 OAM scan, 3 drawing); 0 when off.
    fn mode(&self) -> u8 {
        if !self.lcd_on() {
            0
        } else if self.ly >= VBLANK_LINE {
            1
        } else if self.line_dot < OAM_SCAN_DOTS {
            if self.first_line { 0 } else { 2 }
        } else if self.line_dot < OAM_SCAN_DOTS + self.mode3_len {
            3
        } else {
            0
        }
    }

    /// LY as seen by the LYC comparator and by CPU reads. On line 153 the
    /// hardware reports 0 from the second M-cycle onwards (Pan Docs: LY=153
    /// quirk).
    fn visible_ly(&self) -> u8 {
        if self.ly == LINES_PER_FRAME - 1 && self.line_dot >= 4 {
            0
        } else {
            self.ly
        }
    }

    fn lyc_match(&self) -> bool {
        self.lcd_on() && self.lyc == self.visible_ly()
    }

    /// Advance the PPU by one M-cycle (4 dots).
    pub fn tick_mcycle(&mut self, interrupts: &mut Interrupts) {
        if !self.lcd_on() {
            self.off_dots += 4;
            if self.off_dots >= DOTS_PER_FRAME {
                self.off_dots -= DOTS_PER_FRAME;
                self.frame_ready = true;
            }
            return;
        }
        for _ in 0..4 {
            self.step_dot(interrupts);
        }
        self.update_stat_line(interrupts);
    }

    fn step_dot(&mut self, interrupts: &mut Interrupts) {
        self.line_dot += 1;
        if self.line_dot == DOTS_PER_LINE {
            self.line_dot = 0;
            self.first_line = false;
            self.ly = (self.ly + 1) % LINES_PER_FRAME;
            if self.ly == 0 {
                self.wy_triggered = false;
                self.win_line = 0;
            } else if self.ly == VBLANK_LINE {
                if self.skip_frame {
                    self.skip_frame = false;
                } else {
                    std::mem::swap(&mut self.front, &mut self.back);
                }
                interrupts.request(Interrupt::VBlank);
                self.frame_ready = true;
            }
        } else if self.line_dot == OAM_SCAN_DOTS && self.ly < VBLANK_LINE {
            self.begin_mode3();
        }
    }

    /// Mode 2 -> 3: select sprites, fix the mode 3 length, draw the line.
    fn begin_mode3(&mut self) {
        if self.ly == self.wy {
            self.wy_triggered = true;
        }
        self.scan_oam();
        self.mode3_len = self.compute_mode3_len();
        self.render_line();
    }

    /// Recompute the STAT interrupt line and request STAT on a rising edge
    /// (Pan Docs: "STAT interrupt" - all sources are ORed onto one line, so a
    /// source asserting while another already holds the line high is blocked).
    fn update_stat_line(&mut self, interrupts: &mut Interrupts) {
        let line = self.lcd_on() && {
            let mode = self.mode();
            let sel = self.stat_select;
            (sel & STAT_LYC_IRQ != 0 && self.lyc_match())
                || (sel & STAT_HBLANK_IRQ != 0 && mode == 0)
                || (sel & STAT_VBLANK_IRQ != 0 && mode == 1)
                || (sel & STAT_OAM_IRQ != 0 && mode == 2)
        };
        if line && !self.stat_line {
            interrupts.request(Interrupt::Stat);
        }
        self.stat_line = line;
    }

    pub fn take_frame_ready(&mut self) -> bool {
        std::mem::take(&mut self.frame_ready)
    }

    pub fn framebuffer(&self) -> &Framebuffer {
        &self.front
    }

    /// CPU VRAM read: 0xFF while the PPU is drawing (mode 3).
    pub fn read_vram(&self, addr: u16) -> u8 {
        if self.mode() == 3 {
            0xFF
        } else {
            self.vram[(addr & 0x1FFF) as usize]
        }
    }

    /// CPU VRAM write: ignored while the PPU is drawing (mode 3).
    pub fn write_vram(&mut self, addr: u16, value: u8) {
        if self.mode() != 3 {
            self.vram[(addr & 0x1FFF) as usize] = value;
        }
    }

    /// CPU OAM read: 0xFF during modes 2 and 3.
    pub fn read_oam(&self, addr: u16) -> u8 {
        if self.mode() >= 2 {
            0xFF
        } else {
            self.oam[(addr - 0xFE00) as usize % 0xA0]
        }
    }

    /// CPU OAM write: ignored during modes 2 and 3.
    pub fn write_oam(&mut self, addr: u16, value: u8) {
        if self.mode() < 2 {
            self.oam[(addr - 0xFE00) as usize % 0xA0] = value;
        }
    }

    /// OAM DMA write; bypasses mode blocking. `index` is 0..160.
    pub fn oam_dma_write(&mut self, index: u8, value: u8) {
        self.oam[index as usize % 0xA0] = value;
    }

    pub fn read_reg(&self, addr: u16) -> u8 {
        match addr {
            0xFF40 => self.lcdc,
            0xFF41 => {
                let coincidence = if self.lyc_match() { 0x04 } else { 0 };
                0x80 | self.stat_select | coincidence | self.mode()
            }
            0xFF42 => self.scy,
            0xFF43 => self.scx,
            0xFF44 => self.visible_ly(),
            0xFF45 => self.lyc,
            0xFF47 => self.bgp,
            0xFF48 => self.obp0,
            0xFF49 => self.obp1,
            0xFF4A => self.wy,
            0xFF4B => self.wx,
            _ => 0xFF,
        }
    }

    pub fn write_reg(&mut self, addr: u16, value: u8, interrupts: &mut Interrupts) {
        match addr {
            0xFF40 => {
                let was_on = self.lcd_on();
                self.lcdc = value;
                match (was_on, self.lcd_on()) {
                    (true, false) => self.lcd_turned_off(),
                    (false, true) => self.lcd_turned_on(interrupts),
                    _ => {}
                }
            }
            0xFF41 => {
                self.stat_select = value & STAT_WRITABLE;
                self.update_stat_line(interrupts);
            }
            0xFF42 => self.scy = value,
            0xFF43 => self.scx = value,
            0xFF45 => {
                self.lyc = value;
                self.update_stat_line(interrupts);
            }
            0xFF47 => self.bgp = value,
            0xFF48 => self.obp0 = value,
            0xFF49 => self.obp1 = value,
            0xFF4A => self.wy = value,
            0xFF4B => self.wx = value,
            _ => {}
        }
    }

    /// Pan Docs "LCDC.7": turning the LCD off resets LY and the mode, and the
    /// screen goes blank.
    fn lcd_turned_off(&mut self) {
        self.ly = 0;
        self.line_dot = 0;
        self.off_dots = 0;
        self.stat_line = false;
        self.wy_triggered = false;
        self.win_line = 0;
        self.back.fill(0);
        self.front.fill(0);
    }

    /// Turning the LCD on restarts line 0. The first line has no OAM scan and
    /// the first frame is not displayed (it stays blank).
    fn lcd_turned_on(&mut self, interrupts: &mut Interrupts) {
        self.ly = 0;
        self.line_dot = 0;
        self.first_line = true;
        self.skip_frame = true;
        self.wy_triggered = false;
        self.win_line = 0;
        self.mode3_len = MODE3_BASE_DOTS;
        self.stat_line = false;
        self.update_stat_line(interrupts);
    }
}
