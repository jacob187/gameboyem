//! Pixel processing unit (Pan Docs: "Rendering", "LCD Control", "LCD Status",
//! "Scrolling", "Palettes", "OAM", "STAT" / "STAT interrupt").
//!
//! # Contract (frozen; propose changes to the orchestrator)
//!
//! * [`Ppu::new`] creates the PPU in the DMG post-boot state ([`Ppu::new_cgb`]:
//!   the CGB one).
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
//! The CPU-visible STAT mode bits and the VRAM/OAM access locks follow the
//! internal mode with the offsets measured by mooneye `lcdon_timing-GS` and
//! `lcdon_write_timing-GS`: the mode bits lag the internal mode by one M-cycle
//! (`vis_mode`), OAM reads lock at the line start, and line 0 after an LCD-on
//! write starts one M-cycle in. The interrupt line and the renderer use the
//! internal timeline.
//!
//! On a CGB in colour mode (`Hw::Cgb`) the line is drawn by `render_cgb` into RGB555
//! frames instead ([`Ppu::cgb_framebuffer`]); the shade buffer stays zero. In
//! DMG-compatibility mode the DMG renderer runs unchanged (DECISIONS #36).
//!
//! Rendering goes to a back buffer that is swapped to the front on entering
//! VBlank, so [`Ppu::framebuffer`] never shows a half-drawn frame.

mod render;
mod render_cgb;
#[cfg(test)]
mod tests;

use crate::interrupts::{Interrupt, Interrupts};
use crate::state::{StateError, StateReader, StateWriter, check};
use crate::{CgbFramebuffer, Framebuffer, SCREEN_HEIGHT, SCREEN_WIDTH};

const DOTS_PER_LINE: u16 = 456;
const LINES_PER_FRAME: u8 = 154;
const DOTS_PER_FRAME: u32 = crate::T_CYCLES_PER_FRAME;
const VBLANK_LINE: u8 = SCREEN_HEIGHT as u8;
const OAM_SCAN_DOTS: u16 = 80;
const MODE3_BASE_DOTS: u16 = 172;
/// Dot of line 0 the PPU starts at when the LCD is switched on. The write's
/// own M-cycle has already been ticked, so the line begins one M-cycle in
/// (mooneye `lcdon_timing-GS`: LY, STAT and VRAM/OAM access after LCD on).
const LCD_ON_LINE_DOT: u16 = 4;
/// Dot of line 153 at the first cartridge instruction (see [`Ppu::new`]).
const POST_BOOT_LINE_DOT: u16 = 356;
/// CGB hand-off: line 144 (LY = 0x90, the only value on record, from whichboot.gb
/// on a mainline CGB boot ROM), at an arbitrary dot (DECISIONS #34).
const CGB_POST_BOOT_LINE: u8 = VBLANK_LINE;
const CGB_POST_BOOT_LINE_DOT: u16 = 100;

/// Bytes per VRAM bank.
const VRAM_BANK_SIZE: usize = 0x2000;

/// Bytes of one CGB palette RAM (8 palettes x 4 colours x 2 bytes).
const PALETTE_RAM_SIZE: usize = 64;
/// BCPS / OCPS bit 7: auto-increment the index after a data write.
const PAL_INDEX_AUTO_INC: u8 = 0x80;
/// BCPS / OCPS index bits; bit 6 is unused (reads 1).
const PAL_INDEX_MASK: u8 = 0x3F;
/// Post-boot index registers as stored (bit 6 reads 1, so they read 0xC8 / 0xD0,
/// mooneye `boot_hwio-C`): auto-increment on, index 8 (BG) and 16 (OBJ).
const BCPS_POST_BOOT: u8 = 0x88;
const OCPS_POST_BOOT: u8 = 0x90;
/// RGB555 white, what the LCD shows when it is off.
const CGB_WHITE: u16 = 0x7FFF;
/// OPRI bit 0: 1 = DMG-style (lower X wins), 0 = CGB-style (lower OAM index wins).
const OPRI_X_PRIORITY: u8 = 0x01;

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
    /// Both VRAM banks: bank 0 at `0..0x2000`, bank 1 at `0x2000..0x4000`. A DMG
    /// only ever uses bank 0, so the renderer indexes `vram[..0x2000]` directly;
    /// bank 1 is for CGB attributes and tiles (the CGB renderer indexes it
    /// directly; [`Ppu::vram_bank`] is a read-only view of one bank).
    vram: Box<[u8; 2 * VRAM_BANK_SIZE]>,
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
    /// STAT bit 2: the LY == LYC comparison result. Only re-evaluated while the
    /// LCD is on (the comparison clock stops with it), so it is retained
    /// across LCD off and LYC writes made while the LCD is off.
    lyc_flag: bool,
    /// The mode the CPU sees in STAT bits 0-1: the internal [`Ppu::mode`] as it
    /// was one M-cycle (4 dots) ago. The mode bits lag the timeline the
    /// interrupt line and the renderer use (mooneye `lcdon_timing-GS`,
    /// `intr_2_mode0_timing`, `intr_2_mode3_timing`).
    vis_mode: u8,
    /// Previous level of the STAT interrupt line (for rising-edge detection).
    stat_line: bool,
    back: Box<Framebuffer>,
    front: Box<Framebuffer>,
    frame_ready: bool,
    // ---- CGB additions (DECISIONS #31) ----
    /// CGB hardware (bank 1 exists and is saved), in either CGB mode or
    /// DMG-compatibility mode.
    cgb: bool,
    /// VBK (FF4F) bit 0: the VRAM bank the CPU and OAM DMA see. Always 0 on a DMG
    /// and in DMG-compatibility mode.
    vbk: u8,
    /// BCPS / OCPS (FF68 / FF6A) as stored: bit 7 auto-increment and the 6-bit
    /// index; bit 6 is unused and reads 1. Mapped in compat mode too (they are
    /// plain registers there, the data ports stay unmapped).
    bcps: u8,
    ocps: u8,
    /// OPRI (FF6C) bit 0 (colour mode only).
    opri: u8,
    /// Background / object palette RAM, little-endian RGB555 (colour mode only).
    bg_pal: [u8; PALETTE_RAM_SIZE],
    obj_pal: [u8; PALETTE_RAM_SIZE],
    /// `bg_pal` / `obj_pal` decoded to RGB555 words for the renderer, rebuilt
    /// lazily when the matching `*_stale` flag is set (a BCPD / OCPD write or a
    /// state load). Derived data: never saved.
    bg_decoded: render_cgb::Palettes,
    obj_decoded: render_cgb::Palettes,
    bg_stale: bool,
    obj_stale: bool,
    /// RGB555 back / front frames: `Some` (both) in CGB colour mode only, which
    /// is what [`Ppu::colour`] reports. The DMG `back` / `front` stay all zero in
    /// colour mode.
    cgb_back: Option<Box<CgbFramebuffer>>,
    cgb_front: Option<Box<CgbFramebuffer>>,
    /// The PPU entered HBlank (mode 3 -> 0 on a visible line) since the bus last
    /// took the signal, or the LCD was switched off outside HBlank (SameBoy:
    /// HDMA then still gets its block). It is set whether or not an HDMA is
    /// running; the bus clears it when it starts one. Saved on a CGB only.
    hblank_entered: bool,
    /// The CPU runs in double speed: the PPU gets 2 dots per M-cycle instead of
    /// 4. Set by the bus (it owns the speed), not saved.
    double_speed: bool,
}

/// A frame filled with `colour`.
fn cgb_frame(colour: u16) -> Box<CgbFramebuffer> {
    vec![colour; SCREEN_WIDTH * SCREEN_HEIGHT]
        .into_boxed_slice()
        .try_into()
        .expect("a frame is SCREEN_WIDTH * SCREEN_HEIGHT pixels")
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
    /// The dot within line 153 is pinned by mooneye `boot_hwio-dmgABCmgb`,
    /// which reads STAT 1139 M-cycles after the first instruction and expects
    /// HBlank (mode 0) and then LY = 10: dots 260..=452 all satisfy it, so the
    /// middle of that range is used.
    pub fn new() -> Self {
        Self {
            vram: Box::new([0; 2 * VRAM_BANK_SIZE]),
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
            line_dot: POST_BOOT_LINE_DOT,
            off_dots: 0,
            mode3_len: MODE3_BASE_DOTS,
            first_line: false,
            skip_frame: false,
            wy_triggered: false,
            win_line: 0,
            line_sprites: [0; 10],
            line_sprite_count: 0,
            lyc_flag: true,
            vis_mode: 1,
            stat_line: false,
            back: Box::new([0; SCREEN_WIDTH * SCREEN_HEIGHT]),
            front: Box::new([0; SCREEN_WIDTH * SCREEN_HEIGHT]),
            frame_ready: false,
            cgb: false,
            vbk: 0,
            bcps: 0,
            ocps: 0,
            opri: 0,
            bg_pal: [0; PALETTE_RAM_SIZE],
            obj_pal: [0; PALETTE_RAM_SIZE],
            bg_decoded: [[0; 4]; 8],
            obj_decoded: [[0; 4]; 8],
            bg_stale: true,
            obj_stale: true,
            cgb_back: None,
            cgb_front: None,
            hblank_entered: false,
            double_speed: false,
        }
    }

    /// CGB post-boot state: the DMG one, but VBlank (line 144) instead of the
    /// line-153 quirk window, and two VRAM banks. `colour` selects CGB mode (the
    /// colour PPU) over DMG-compatibility mode.
    ///
    /// Palette RAM is white (the boot ROM clears it; SameBoy randomises it before
    /// the boot ROM runs, Pan Docs leaves the OBJ half undefined): both halves
    /// are 0xFF so the state is deterministic. DECISIONS #36.
    pub(crate) fn new_cgb(colour: bool) -> Self {
        Self {
            ly: CGB_POST_BOOT_LINE,
            line_dot: CGB_POST_BOOT_LINE_DOT,
            // LYC (0) != 144, and the STAT select bits are clear.
            lyc_flag: false,
            cgb: true,
            bcps: BCPS_POST_BOOT,
            ocps: OCPS_POST_BOOT,
            bg_pal: [0xFF; PALETTE_RAM_SIZE],
            obj_pal: [0xFF; PALETTE_RAM_SIZE],
            cgb_back: colour.then(|| cgb_frame(CGB_WHITE)),
            cgb_front: colour.then(|| cgb_frame(CGB_WHITE)),
            ..Self::new()
        }
    }

    /// CGB colour mode (`Hw::Cgb`): the colour renderer, palette RAM and OPRI
    /// exist. False on a DMG and in DMG-compatibility mode, whose output is the
    /// DMG one (DECISIONS #36). Derived from the RGB555 frames, which exist
    /// exactly then.
    fn colour(&self) -> bool {
        self.cgb_back.is_some()
    }

    /// One whole VRAM bank (0 or 1), unaffected by VBK and by PPU mode. A
    /// read-only view (tests use it); the renderer indexes `vram` directly.
    pub fn vram_bank(&self, bank: usize) -> &[u8; VRAM_BANK_SIZE] {
        let start = (bank & 1) * VRAM_BANK_SIZE;
        self.vram[start..start + VRAM_BANK_SIZE]
            .try_into()
            .expect("a bank is VRAM_BANK_SIZE bytes")
    }

    /// Offset of the CPU-selected bank in `vram`.
    fn vram_base(&self) -> usize {
        usize::from(self.vbk) * VRAM_BANK_SIZE
    }

    /// VBK (FF4F) as read: bit 0 is the bank, the rest read 1.
    pub(crate) fn read_vbk(&self) -> u8 {
        0xFE | self.vbk
    }

    pub(crate) fn write_vbk(&mut self, value: u8) {
        self.vbk = value & 1;
    }

    /// BCPS / OCPS as read: bit 6 is unused and reads 1.
    pub(crate) fn read_bcps(&self) -> u8 {
        0x40 | self.bcps
    }

    pub(crate) fn read_ocps(&self) -> u8 {
        0x40 | self.ocps
    }

    pub(crate) fn write_bcps(&mut self, value: u8) {
        self.bcps = value & (PAL_INDEX_AUTO_INC | PAL_INDEX_MASK);
    }

    pub(crate) fn write_ocps(&mut self, value: u8) {
        self.ocps = value & (PAL_INDEX_AUTO_INC | PAL_INDEX_MASK);
    }

    /// BCPD (FF69) / OCPD (FF6B) read: 0xFF while palette RAM is locked. It is
    /// locked exactly like VRAM (Pan Docs: "inaccessible during mode 3").
    pub(crate) fn read_pal_data(&self, obj: bool) -> u8 {
        if self.vram_read_blocked() {
            return 0xFF;
        }
        if obj {
            self.obj_pal[usize::from(self.ocps & PAL_INDEX_MASK)]
        } else {
            self.bg_pal[usize::from(self.bcps & PAL_INDEX_MASK)]
        }
    }

    /// BCPD / OCPD write. While locked the byte is dropped but the index still
    /// auto-increments (SameBoy `cgb_palettes_blocked`).
    pub(crate) fn write_pal_data(&mut self, obj: bool, value: u8) {
        let blocked = self.vram_write_blocked();
        let (index, ram, stale) = if obj {
            (&mut self.ocps, &mut self.obj_pal, &mut self.obj_stale)
        } else {
            (&mut self.bcps, &mut self.bg_pal, &mut self.bg_stale)
        };
        if !blocked {
            ram[usize::from(*index & PAL_INDEX_MASK)] = value;
            *stale = true;
        }
        if *index & PAL_INDEX_AUTO_INC != 0 {
            *index = PAL_INDEX_AUTO_INC | (*index).wrapping_add(1) & PAL_INDEX_MASK;
        }
    }

    /// OPRI (FF6C) as read.
    pub(crate) fn read_opri(&self) -> u8 {
        0xFE | self.opri
    }

    pub(crate) fn write_opri(&mut self, value: u8) {
        self.opri = value & OPRI_X_PRIORITY;
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

    /// Re-run the LY == LYC comparison. A no-op while the LCD is off.
    fn refresh_lyc_flag(&mut self) {
        if self.lcd_on() {
            self.lyc_flag = self.lyc == self.visible_ly();
        }
    }

    /// STAT bit 2 as the CPU reads it: the comparator output is blanked
    /// during the first M-cycle of every line, so it reads 0 there even if the
    /// new LY equals LYC (mooneye `lcdon_timing-GS`). Line 153 is exempt: its
    /// LY=153 compare happens in that first M-cycle (LY then reads 0). The
    /// interrupt source uses the unblanked flag.
    fn coincidence(&self) -> bool {
        let blanked = self.lcd_on() && self.line_dot < 4 && self.ly != LINES_PER_FRAME - 1;
        self.lyc_flag && !blanked
    }

    /// Advance the PPU by one M-cycle (4 dots). Forced inline into the bus's
    /// plain tick path: without it the DMG loses about 8% of its speed (the
    /// generic body made the compiler stop inlining it).
    #[inline(always)]
    pub fn tick_mcycle(&mut self, interrupts: &mut Interrupts) {
        self.tick_dots::<4>(interrupts);
    }

    /// Advance the PPU by one CPU M-cycle in double speed (2 dots). The PPU
    /// keeps its real-time rate, so a CPU cycle is half as many dots
    /// (DECISIONS #38).
    pub(crate) fn tick_mcycle_double(&mut self, interrupts: &mut Interrupts) {
        self.tick_dots::<2>(interrupts);
    }

    /// `DOTS` dots per M-cycle: 4 at normal speed, 2 in double speed. The
    /// windows below measured in literal dots (the first M-cycle of a line, the
    /// LY=153 quirk) are properties of the dot clock and stay as they are.
    #[inline(always)]
    fn tick_dots<const DOTS: u16>(&mut self, interrupts: &mut Interrupts) {
        if !self.lcd_on() {
            self.off_dots += u32::from(DOTS);
            if self.off_dots >= DOTS_PER_FRAME {
                self.off_dots -= DOTS_PER_FRAME;
                self.frame_ready = true;
            }
            return;
        }
        self.vis_mode = self.mode();
        let dot = self.line_dot;
        let next = dot + DOTS;
        if next < DOTS_PER_LINE && !(dot < OAM_SCAN_DOTS && next >= OAM_SCAN_DOTS) {
            // None of the dots ends the line or starts mode 3, so every
            // `step_dot` would only have incremented `line_dot`.
            self.line_dot = next;
            // The LYC flag and STAT line were last evaluated at `dot` (every
            // state change calls `refresh_lyc_flag` / `update_stat_line`).
            // They depend on `line_dot` only through the line-start window
            // (dots 0..4), the mode 2 -> 3 edge (excluded above) and the mode
            // 3 -> 0 edge, so with none of those crossed they cannot change.
            let mode3_end = OAM_SCAN_DOTS + self.mode3_len;
            // A CGB also re-evaluates when entering the line-144 STAT pulse
            // window, the last M-cycle of line 143 (see `stat_line_level_for`).
            let pulse_start = DOTS_PER_LINE - DOTS;
            if dot >= 4
                && !(dot < mode3_end && next >= mode3_end)
                && !(next >= pulse_start
                    && dot < pulse_start
                    && self.cgb
                    && self.ly == VBLANK_LINE - 1)
            {
                debug_assert_eq!(self.lyc_flag, self.lyc == self.visible_ly());
                debug_assert_eq!(self.stat_line, self.stat_line_level());
                return;
            }
            // The HBlank edge of a visible line (a no-op for the other windows).
            if dot < mode3_end && next >= mode3_end && self.ly < VBLANK_LINE {
                self.hblank_entered = true;
            }
        } else {
            for _ in 0..DOTS {
                self.step_dot(interrupts);
            }
        }
        self.refresh_lyc_flag();
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
                    std::mem::swap(&mut self.cgb_front, &mut self.cgb_back);
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
        if self.first_line {
            // No OAM scan on the first line after LCD on: no sprites are
            // selected, so there are no sprite penalties either.
            self.line_sprite_count = 0;
        } else {
            self.scan_oam();
        }
        self.mode3_len = self.compute_mode3_len();
        self.render_line();
    }

    /// Recompute the STAT interrupt line and request STAT on a rising edge
    /// (Pan Docs: "STAT interrupt" - all sources are ORed onto one line, so a
    /// source asserting while another already holds the line high is blocked).
    ///
    /// The LYC source follows the latched comparison flag, which survives the
    /// LCD being off (mooneye `stat_lyc_onoff`). The mode sources need the LCD
    /// on. Line 144 also pulses the mode 2 (OAM) source as VBlank begins
    /// (mooneye `vblank_stat_intr-GS`).
    ///
    /// The level is latched even with the LCD off (the LYC term survives it),
    /// but a rising edge only requests the interrupt while the LCD is on.
    fn update_stat_line(&mut self, interrupts: &mut Interrupts) {
        let line = self.stat_line_level();
        if line && !self.stat_line && self.lcd_on() {
            interrupts.request(Interrupt::Stat);
        }
        self.stat_line = line;
    }

    /// The current level of the STAT interrupt line (see [`Self::update_stat_line`]).
    fn stat_line_level(&self) -> bool {
        self.stat_line_level_for(self.stat_select)
    }

    /// The STAT line level if the select bits were `sel`.
    ///
    /// The first 80 dots of the first line after LCD on report mode 0 to the
    /// CPU but feed no mode source into the line (SameBoy runs them with
    /// `mode_for_interrupt = -1`), so an HBlank select does not fire there.
    fn stat_line_level_for(&self, sel: u8) -> bool {
        let mode_line = self.lcd_on() && !(self.first_line && self.line_dot < OAM_SCAN_DOTS) && {
            let mode = self.mode();
            // DMG: the M-cycle VBlank starts in. CGB: the one before it (mooneye
            // `vblank_stat_intr-C`: the STAT interrupt precedes VBlank by a
            // cycle), which is 4 dots at normal speed and 2 in double speed.
            let vblank_oam = if self.cgb {
                let m_cycle = if self.double_speed { 2 } else { 4 };
                self.ly == VBLANK_LINE - 1 && self.line_dot >= DOTS_PER_LINE - m_cycle
            } else {
                self.ly == VBLANK_LINE && self.line_dot < 4
            };
            (sel & STAT_HBLANK_IRQ != 0 && mode == 0)
                || (sel & STAT_VBLANK_IRQ != 0 && mode == 1)
                || (sel & STAT_OAM_IRQ != 0 && (mode == 2 || vblank_oam))
        };
        mode_line || (sel & STAT_LYC_IRQ != 0 && self.lyc_flag)
    }

    pub fn take_frame_ready(&mut self) -> bool {
        std::mem::take(&mut self.frame_ready)
    }

    /// The last completed frame as shades. In CGB colour mode this is all zero:
    /// the frame is [`Ppu::cgb_framebuffer`] (DECISIONS #36).
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.front
    }

    /// The last completed frame in RGB555; `Some` only in CGB colour mode.
    pub fn cgb_framebuffer(&self) -> Option<&CgbFramebuffer> {
        self.cgb_front.as_deref()
    }

    /// The mode seen by CPU reads of STAT; 0 while the LCD is off.
    fn stat_mode(&self) -> u8 {
        if self.lcd_on() { self.vis_mode } else { 0 }
    }

    /// VRAM is locked to CPU reads while the displayed mode is 3, and from the
    /// internal mode 3 start too, except on the first line after LCD on where
    /// the lock starts with the displayed mode (mooneye `lcdon_timing-GS`).
    fn vram_read_blocked(&self) -> bool {
        self.stat_mode() == 3 || (self.mode() == 3 && !self.first_line)
    }

    /// VRAM writes are locked exactly while the displayed mode is 3.
    fn vram_write_blocked(&self) -> bool {
        self.stat_mode() == 3
    }

    /// OAM reads are locked from the internal mode 2 start (a cycle before the
    /// mode bits change) through displayed mode 3.
    fn oam_read_blocked(&self) -> bool {
        self.stat_mode() >= 2 || self.mode() == 2
    }

    /// OAM writes are locked during displayed mode 2 and 3, except for a
    /// one-cycle gap where displayed mode 2 meets internal mode 3.
    fn oam_write_blocked(&self) -> bool {
        match self.stat_mode() {
            3 => true,
            2 => self.mode() == 2,
            _ => false,
        }
    }

    /// CPU VRAM read: 0xFF while the PPU is drawing (mode 3).
    pub fn read_vram(&self, addr: u16) -> u8 {
        if self.vram_read_blocked() {
            0xFF
        } else {
            self.vram[self.vram_base() + (addr & 0x1FFF) as usize]
        }
    }

    /// CPU VRAM write: ignored while the PPU is drawing (mode 3).
    pub fn write_vram(&mut self, addr: u16, value: u8) {
        if !self.vram_write_blocked() {
            let base = self.vram_base();
            self.vram[base + (addr & 0x1FFF) as usize] = value;
        }
    }

    /// CPU OAM read: 0xFF during modes 2 and 3.
    pub fn read_oam(&self, addr: u16) -> u8 {
        if self.oam_read_blocked() {
            0xFF
        } else {
            self.oam[(addr - 0xFE00) as usize % 0xA0]
        }
    }

    /// CPU OAM write: ignored during modes 2 and 3.
    pub fn write_oam(&mut self, addr: u16, value: u8) {
        if !self.oam_write_blocked() {
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
                let coincidence = if self.coincidence() { 0x04 } else { 0 };
                0x80 | self.stat_select | coincidence | self.stat_mode()
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
                let outside_hblank = was_on && !self.in_hdma_window();
                self.lcdc = value;
                match (was_on, self.lcd_on()) {
                    (true, false) => self.lcd_turned_off(interrupts, outside_hblank),
                    (false, true) => self.lcd_turned_on(interrupts),
                    _ => {}
                }
            }
            0xFF41 => {
                // DMG quirk (Pan Docs "STAT"): the write behaves as if $FF
                // were written for one M-cycle before the real value, so any
                // source that is currently true raises the line. A CGB (in
                // either mode) does not have it (SameBoy, DECISIONS #36).
                if !self.cgb
                    && self.lcd_on()
                    && !self.stat_line
                    && self.stat_line_level_for(STAT_WRITABLE)
                {
                    interrupts.request(Interrupt::Stat);
                    self.stat_line = true;
                }
                self.stat_select = value & STAT_WRITABLE;
                self.update_stat_line(interrupts);
            }
            0xFF42 => self.scy = value,
            0xFF43 => self.scx = value,
            0xFF45 => {
                self.lyc = value;
                self.refresh_lyc_flag();
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
    ///
    /// SameBoy (`GB_lcd_off`): an armed HDMA still gets a block if the LCD goes
    /// off outside HBlank (`outside_hblank`), because the STAT mode then reads 0.
    fn lcd_turned_off(&mut self, interrupts: &mut Interrupts, outside_hblank: bool) {
        if outside_hblank {
            self.hblank_entered = true;
        }
        self.ly = 0;
        self.line_dot = 0;
        self.off_dots = 0;
        self.wy_triggered = false;
        self.win_line = 0;
        self.back.fill(0);
        self.front.fill(0);
        for fb in [&mut self.cgb_back, &mut self.cgb_front]
            .into_iter()
            .flatten()
        {
            fb.fill(CGB_WHITE);
        }
        self.update_stat_line(interrupts);
    }

    /// Turning the LCD on restarts line 0. The first line has no OAM scan and
    /// the first frame is not displayed (it stays blank).
    fn lcd_turned_on(&mut self, interrupts: &mut Interrupts) {
        self.ly = 0;
        // The write's own M-cycle is already ticked: 4 dots at normal speed, 2
        // in double speed.
        self.line_dot = if self.double_speed {
            LCD_ON_LINE_DOT / 2
        } else {
            LCD_ON_LINE_DOT
        };
        self.vis_mode = 0;
        self.first_line = true;
        self.skip_frame = true;
        self.wy_triggered = false;
        self.win_line = 0;
        self.mode3_len = MODE3_BASE_DOTS;
        self.refresh_lyc_flag();
        self.update_stat_line(interrupts);
    }
}

/// Bus-facing hooks for CGB double speed and HDMA.
impl Ppu {
    pub(crate) fn set_double_speed(&mut self, double: bool) {
        self.double_speed = double;
    }

    /// Take the "entered HBlank" signal (see [`Ppu::hblank_entered`] the field).
    pub(crate) fn take_hblank_entered(&mut self) -> bool {
        std::mem::take(&mut self.hblank_entered)
    }

    /// Raise (HDMA started in HBlank) or clear (HDMA started elsewhere, so a
    /// stale signal from an earlier line cannot fire it) the signal.
    pub(crate) fn set_hblank_entered(&mut self, signalled: bool) {
        self.hblank_entered = signalled;
    }

    /// The HDMA copies a block now when it is armed: the internal mode is 0.
    /// That is HBlank on a visible line, the LCD being off, and dots 4..80 of
    /// the first line after LCD on, where the STAT mode also reads 0 (SameBoy
    /// starts an FF55 HBlank transfer when `STAT & 3 == 0`, outside its
    /// line-start `display_state` 7, which the first line does not have, and
    /// `GB_lcd_off` arms one only when `STAT & 3` is non-zero).
    pub(crate) fn in_hdma_window(&self) -> bool {
        self.mode() == 0
    }

    /// HDMA write to VRAM: into the CPU-selected bank, whatever the PPU mode.
    /// `offset` is 0..0x2000.
    pub(crate) fn hdma_write_vram(&mut self, offset: u16, value: u8) {
        let base = self.vram_base();
        self.vram[base + usize::from(offset & 0x1FFF)] = value;
    }
}

/// Bus-facing queries for OAM DMA and the unusable FEA0-FEFF range.
impl Ppu {
    /// VRAM read for the OAM DMA engine, which is not gated by PPU mode (Pan Docs "OAM DMA Transfer").
    pub(crate) fn dma_read_vram(&self, addr: u16) -> u8 {
        self.vram[self.vram_base() + (addr & 0x1FFF) as usize]
    }

    /// True while the CPU cannot read OAM (modes 2 and 3; Pan Docs "FEA0-FEFF range").
    pub(crate) fn oam_blocked(&self) -> bool {
        self.oam_read_blocked()
    }
}

impl Ppu {
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.bytes(&self.vram[..self.vram_len()]);
        w.bytes(&self.oam);
        for v in [
            self.lcdc,
            self.stat_select,
            self.scy,
            self.scx,
            self.ly,
            self.lyc,
            self.bgp,
            self.obp0,
            self.obp1,
            self.wy,
            self.wx,
        ] {
            w.u8(v);
        }
        w.u16(self.line_dot);
        w.u32(self.off_dots);
        w.u16(self.mode3_len);
        w.bool(self.first_line);
        w.bool(self.skip_frame);
        w.bool(self.wy_triggered);
        w.u8(self.win_line);
        w.bytes(&self.line_sprites);
        w.u8(self.line_sprite_count);
        w.bool(self.lyc_flag);
        w.u8(self.vis_mode);
        w.bool(self.stat_line);
        // In colour mode the shade frames are all zero by design (DECISIONS
        // #36), so they are not saved; the RGB555 frames below carry the picture.
        if !self.colour() {
            w.bytes(&self.back[..]);
            w.bytes(&self.front[..]);
        }
        w.bool(self.frame_ready);
        // CGB additions.
        if self.cgb {
            w.u8(self.vbk);
            w.u8(self.bcps);
            w.u8(self.ocps);
            w.bool(self.hblank_entered);
        }
        if let (Some(back), Some(front)) = (&self.cgb_back, &self.cgb_front) {
            w.u8(self.opri);
            w.bytes(&self.bg_pal);
            w.bytes(&self.obj_pal);
            for &c in back.iter().chain(front.iter()) {
                w.u16(c);
            }
        }
    }

    /// Bytes of `vram` in use: both banks on a CGB, bank 0 on a DMG.
    fn vram_len(&self) -> usize {
        if self.cgb {
            2 * VRAM_BANK_SIZE
        } else {
            VRAM_BANK_SIZE
        }
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let len = self.vram_len();
        r.bytes_into(&mut self.vram[..len])?;
        r.bytes_into(&mut self.oam)?;
        self.lcdc = r.u8()?;
        self.stat_select = r.u8()?;
        check(self.stat_select & !STAT_WRITABLE == 0, "STAT")?;
        self.scy = r.u8()?;
        self.scx = r.u8()?;
        self.ly = r.u8()?;
        check(self.ly < LINES_PER_FRAME, "LY")?;
        self.lyc = r.u8()?;
        self.bgp = r.u8()?;
        self.obp0 = r.u8()?;
        self.obp1 = r.u8()?;
        self.wy = r.u8()?;
        self.wx = r.u8()?;
        self.line_dot = r.u16()?;
        check(self.line_dot < DOTS_PER_LINE, "line dot")?;
        self.off_dots = r.u32()?;
        check(self.off_dots < DOTS_PER_FRAME, "LCD-off dots")?;
        self.mode3_len = r.u16()?;
        check(
            self.mode3_len <= DOTS_PER_LINE - OAM_SCAN_DOTS,
            "mode 3 length",
        )?;
        self.first_line = r.bool()?;
        self.skip_frame = r.bool()?;
        self.wy_triggered = r.bool()?;
        self.win_line = r.u8()?;
        r.bytes_into(&mut self.line_sprites)?;
        self.line_sprite_count = r.u8()?;
        check(
            usize::from(self.line_sprite_count) <= self.line_sprites.len(),
            "sprite count",
        )?;
        check(
            self.line_sprites
                .iter()
                .all(|&i| usize::from(i) < self.oam.len() / 4),
            "sprite index",
        )?;
        self.lyc_flag = r.bool()?;
        self.vis_mode = r.u8()?;
        check(self.vis_mode <= 3, "STAT mode")?;
        self.stat_line = r.bool()?;
        if self.colour() {
            self.back.fill(0);
            self.front.fill(0);
        } else {
            r.bytes_into(&mut self.back[..])?;
            r.bytes_into(&mut self.front[..])?;
            check(
                self.back.iter().chain(self.front.iter()).all(|&s| s <= 3),
                "framebuffer shade",
            )?;
        }
        self.frame_ready = r.bool()?;
        if self.cgb {
            self.vbk = r.u8()?;
            // Compatibility mode drops VBK writes, so there it stays bank 0.
            check(self.vbk <= u8::from(self.colour()), "VBK")?;
            self.bcps = r.u8()?;
            self.ocps = r.u8()?;
            check(
                (self.bcps | self.ocps) & !(PAL_INDEX_AUTO_INC | PAL_INDEX_MASK) == 0,
                "palette index",
            )?;
            self.hblank_entered = r.bool()?;
        }
        if let (Some(back), Some(front)) =
            (self.cgb_back.as_deref_mut(), self.cgb_front.as_deref_mut())
        {
            self.opri = r.u8()?;
            check(self.opri <= OPRI_X_PRIORITY, "OPRI")?;
            r.bytes_into(&mut self.bg_pal)?;
            r.bytes_into(&mut self.obj_pal)?;
            self.bg_stale = true;
            self.obj_stale = true;
            for fb in [back, front] {
                for c in fb.iter_mut() {
                    *c = r.u16()?;
                    check(*c <= CGB_WHITE, "framebuffer colour")?;
                }
            }
        }
        Ok(())
    }
}
