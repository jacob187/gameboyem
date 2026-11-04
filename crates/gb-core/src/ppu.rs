//! Pixel processing unit (Pan Docs: "Rendering", "LCD Control", "LCD Status",
//! "Scrolling", "Palettes", "OAM").
//!
//! # Contract (frozen; propose changes to the orchestrator)
//!
//! * [`Ppu::tick_mcycle`] advances 4 dots and requests VBlank / STAT interrupts.
//! * [`Ppu::take_frame_ready`] returns true once per frame: when LY reaches 144
//!   with the LCD on, and every 70224 dots while the LCD is off (so
//!   `GameBoy::step_frame` always terminates).
//! * [`Ppu::framebuffer`] is the last *completed* frame: 160x144 shades 0..=3
//!   after BGP/OBP mapping. The LCD-off frame is blank (shade 0).
//! * VRAM/OAM CPU accessors apply mode blocking; [`Ppu::oam_dma_write`] does not.
//!
//! Phase 1 stub: register storage and LY/VBlank timing only; nothing is drawn.

use crate::interrupts::{Interrupt, Interrupts};
use crate::{Framebuffer, SCREEN_HEIGHT, SCREEN_WIDTH};

const DOTS_PER_LINE: u16 = 456;
const LINES_PER_FRAME: u8 = 154;
const DOTS_PER_FRAME: u32 = crate::T_CYCLES_PER_FRAME;

pub struct Ppu {
    vram: Box<[u8; 0x2000]>,
    oam: [u8; 0xA0],
    lcdc: u8,
    stat: u8,
    scy: u8,
    scx: u8,
    ly: u8,
    lyc: u8,
    bgp: u8,
    obp0: u8,
    obp1: u8,
    wy: u8,
    wx: u8,
    line_dot: u16,
    /// Dots elapsed while the LCD is off (for the LCD-off frame cadence).
    off_dots: u32,
    front: Box<Framebuffer>,
    frame_ready: bool,
}

impl Default for Ppu {
    fn default() -> Self {
        Self::new()
    }
}

impl Ppu {
    pub fn new() -> Self {
        Self {
            vram: Box::new([0; 0x2000]),
            oam: [0; 0xA0],
            lcdc: 0x91,
            stat: 0x85,
            scy: 0,
            scx: 0,
            ly: 0,
            lyc: 0,
            bgp: 0xFC,
            obp0: 0xFF,
            obp1: 0xFF,
            wy: 0,
            wx: 0,
            line_dot: 0,
            off_dots: 0,
            front: Box::new([0; SCREEN_WIDTH * SCREEN_HEIGHT]),
            frame_ready: false,
        }
    }

    fn lcd_on(&self) -> bool {
        self.lcdc & 0x80 != 0
    }

    pub fn tick_mcycle(&mut self, interrupts: &mut Interrupts) {
        if !self.lcd_on() {
            self.off_dots += 4;
            if self.off_dots >= DOTS_PER_FRAME {
                self.off_dots -= DOTS_PER_FRAME;
                self.frame_ready = true;
            }
            return;
        }
        self.line_dot += 4;
        if self.line_dot >= DOTS_PER_LINE {
            self.line_dot -= DOTS_PER_LINE;
            self.ly = (self.ly + 1) % LINES_PER_FRAME;
            if self.ly == SCREEN_HEIGHT as u8 {
                interrupts.request(Interrupt::VBlank);
                self.frame_ready = true;
            }
        }
    }

    pub fn take_frame_ready(&mut self) -> bool {
        std::mem::take(&mut self.frame_ready)
    }

    pub fn framebuffer(&self) -> &Framebuffer {
        &self.front
    }

    pub fn read_vram(&self, addr: u16) -> u8 {
        self.vram[(addr & 0x1FFF) as usize]
    }

    pub fn write_vram(&mut self, addr: u16, value: u8) {
        self.vram[(addr & 0x1FFF) as usize] = value;
    }

    pub fn read_oam(&self, addr: u16) -> u8 {
        self.oam[(addr - 0xFE00) as usize % 0xA0]
    }

    pub fn write_oam(&mut self, addr: u16, value: u8) {
        self.oam[(addr - 0xFE00) as usize % 0xA0] = value;
    }

    /// OAM DMA write; bypasses mode blocking. `index` is 0..160.
    pub fn oam_dma_write(&mut self, index: u8, value: u8) {
        self.oam[index as usize % 0xA0] = value;
    }

    pub fn read_reg(&self, addr: u16) -> u8 {
        match addr {
            0xFF40 => self.lcdc,
            0xFF41 => self.stat | 0x80,
            0xFF42 => self.scy,
            0xFF43 => self.scx,
            0xFF44 => self.ly,
            0xFF45 => self.lyc,
            0xFF47 => self.bgp,
            0xFF48 => self.obp0,
            0xFF49 => self.obp1,
            0xFF4A => self.wy,
            0xFF4B => self.wx,
            _ => 0xFF,
        }
    }

    pub fn write_reg(&mut self, addr: u16, value: u8, _interrupts: &mut Interrupts) {
        match addr {
            0xFF40 => {
                let was_on = self.lcd_on();
                self.lcdc = value;
                if was_on && !self.lcd_on() {
                    self.ly = 0;
                    self.line_dot = 0;
                    self.off_dots = 0;
                }
            }
            0xFF41 => self.stat = (self.stat & 0x07) | (value & 0x78),
            0xFF42 => self.scy = value,
            0xFF43 => self.scx = value,
            0xFF45 => self.lyc = value,
            0xFF47 => self.bgp = value,
            0xFF48 => self.obp0 = value,
            0xFF49 => self.obp1 = value,
            0xFF4A => self.wy = value,
            0xFF4B => self.wx = value,
            _ => {}
        }
    }
}
