//! The cartridge GPIO port at 0x0800_00C4 (GBATEK "GBA Cart I/O Port (GPIO)") and
//! the S-3511 serial protocol that the RTC speaks over it.
//!
//! Registers (16-bit, in ROM space): data 0x0800_00C4 (bits 0-3), direction
//! 0x0800_00C6 (bits 0-3, 1 = output from the GBA), control 0x0800_00C8 (bit 0 =
//! reads enabled; otherwise the addresses read as plain ROM). The RTC uses bit 0
//! SCK, bit 1 SIO, bit 2 CS. Only 16/32-bit writes reach the port (STRB is
//! ignored). The solar sensor, rumble and gyro that share the port on other carts
//! are not modelled; their pins read 0.
//!
//! **Protocol** (the wire format every emulator and game agrees on): CS rises to
//! start a transfer. A command byte follows, most significant bit first, each bit
//! sampled when SCK rises: `0110 ccc r` where `ccc` is the register (0 force
//! reset, 1 control, 2 datetime, 3 time, 4/5/7 alarm and free registers that
//! read FFh, 6 force IRQ) and `r` is 1 for a read. Data bytes follow least
//! significant bit first: written by the GBA and sampled at SCK rising, or read
//! from the chip, which presents the next bit when SCK falls so it is valid
//! from the following rising edge. CS falling ends the transfer, discarding an
//! unfinished write.
//!
//! The port is edge-driven from register writes; nothing here ticks. The clock
//! is caught up to the master clock only when a command starts.

use super::rtc::{CTRL_POWER, FOOTER_LEN, Rtc};

/// `--rtc`: whether the cartridge has a GPIO port with a clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RtcMode {
    /// The port stays inert (all reads are ROM) until the game writes a value
    /// that looks like GPIO setup to 0x0800_00C4..=C9 (DECISIONS #66). Waking it
    /// is harmless for a game that is not using a clock: reads still return ROM
    /// until the control register enables them, and the save image gains a
    /// footer only once a valid RTC command arrives.
    #[default]
    Auto,
    /// Present from power-on.
    On,
    /// Absent: writes are ignored and the addresses always read as ROM.
    Off,
}

const SCK: u8 = 1;
const SIO: u8 = 2;
const CS: u8 = 4;

const REG_DATA: u32 = 0xC4;
const REG_DIR: u32 = 0xC6;
const REG_CTRL: u32 = 0xC8;

enum Phase {
    /// CS is low, or the command was not an S-3511 command.
    Idle,
    /// Collecting the command byte, MSB first.
    Command { value: u8, bits: u8 },
    /// Collecting `len` data bytes, LSB first, for register `reg`.
    Write {
        reg: u8,
        len: u8,
        buf: [u8; 7],
        got: u8,
        bits: u8,
    },
    /// Presenting `len` bytes, LSB first.
    Read {
        buf: [u8; 7],
        len: u8,
        byte: u8,
        bit: u8,
    },
}

/// What a port write did beyond changing its own registers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpioEffect {
    /// The clock pulsed its IRQ output (force-IRQ command): raise the Game Pak
    /// interrupt.
    pub irq: bool,
    /// A clock register changed (a write, a reset, or the power flag being
    /// cleared by a read), so the saved image changed.
    pub rtc_changed: bool,
}

pub(super) struct Gpio {
    mode: RtcMode,
    /// The port exists: always for `On`, after the first plausible write for
    /// `Auto`, never for `Off`.
    live: bool,
    data: u8,
    dir: u8,
    control: u8,
    /// The pin levels the GBA drives (`data & dir`) at the last write, for edge
    /// detection.
    driven: u8,
    phase: Phase,
    /// The level the RTC drives on SIO while it is being read.
    sio: bool,
    rtc: Rtc,
    /// The clock has been used: the chip received a valid command byte (CS high
    /// and a full `0110 ccc r`), or its state was loaded from a save footer. Only
    /// then does the save image carry the footer, so a game that never talks to a
    /// clock keeps a chip-sized `.sav` (code-review K#2, K#3).
    used: bool,
}

impl Gpio {
    pub fn new(mode: RtcMode) -> Self {
        Self {
            mode,
            live: mode == RtcMode::On,
            data: 0,
            dir: 0,
            control: 0,
            driven: 0,
            phase: Phase::Idle,
            sio: false,
            rtc: Rtc::power_on(),
            used: false,
        }
    }

    pub fn live(&self) -> bool {
        self.live
    }

    /// Whether any register currently reads back instead of ROM (the port exists
    /// and its control register enabled reads): what `read16` tests first.
    pub fn reads_enabled(&self) -> bool {
        self.live && self.control & 1 != 0
    }

    pub fn rtc(&self) -> &Rtc {
        &self.rtc
    }

    pub fn rtc_mut(&mut self) -> &mut Rtc {
        &mut self.rtc
    }

    /// Install a clock restored from a save footer.
    pub fn load_rtc(&mut self, rtc: Rtc) {
        self.rtc = rtc;
        self.used = true;
        self.phase = Phase::Idle;
    }

    /// The footer to append to the save image at cycle `now`, once the clock has
    /// been used or was loaded from a save. With `--rtc off` it is carried over frozen.
    pub fn footer(&self, now: u64) -> Option<[u8; FOOTER_LEN]> {
        if !self.used {
            return None;
        }
        Some(if self.mode == RtcMode::Off {
            self.rtc.footer()
        } else {
            self.rtc.at(now).footer()
        })
    }

    /// The register at even ROM offset `reg` as a halfword, or `None` where the
    /// address reads as ROM (port absent, reads not enabled, or not a register).
    pub fn read16(&self, reg: u32) -> Option<u16> {
        if !self.reads_enabled() {
            return None;
        }
        match reg {
            REG_DATA => {
                let input = if self.dir & SIO == 0 && self.sio {
                    SIO
                } else {
                    0
                };
                Some(u16::from((self.data & self.dir) | input))
            }
            REG_DIR => Some(u16::from(self.dir)),
            REG_CTRL => Some(u16::from(self.control)),
            _ => None,
        }
    }

    /// Whether `v` written to `reg` looks like GPIO setup rather than a stray
    /// store: data and direction use 4 bits, control 1.
    fn plausible(reg: u32, v: u16) -> bool {
        match reg {
            REG_DATA | REG_DIR => v & !0xF == 0,
            REG_CTRL => v & !1 == 0,
            _ => false,
        }
    }

    /// A 16-bit write to the even ROM offset `reg` at master-clock cycle `now`.
    pub fn write16(&mut self, reg: u32, v: u16, now: u64) -> GpioEffect {
        let mut fx = GpioEffect::default();
        if !self.live {
            if self.mode != RtcMode::Auto || !Self::plausible(reg, v) {
                return fx;
            }
            self.live = true;
        }
        match reg {
            REG_DATA => {
                self.data = (v & 0xF) as u8;
                self.pins_changed(now, &mut fx);
            }
            REG_DIR => {
                self.dir = (v & 0xF) as u8;
                self.pins_changed(now, &mut fx);
            }
            REG_CTRL => self.control = (v & 1) as u8,
            _ => {}
        }
        fx
    }

    /// A 32-bit write to the word-aligned ROM offset `reg`. At 0xC4 it sets data
    /// (low half) and direction (high half) together: the pin state is computed
    /// once from both, so no intermediate "new data, old direction" state can
    /// clock the chip (code-review K#1). At 0xC8 only the control register (low
    /// half) is a register.
    pub fn write32(&mut self, reg: u32, v: u32, now: u64) -> GpioEffect {
        let mut fx = GpioEffect::default();
        if reg != REG_DATA {
            return self.write16(reg, v as u16, now);
        }
        let (data, dir) = (v as u16, (v >> 16) as u16);
        if !self.live {
            if self.mode != RtcMode::Auto
                || !Self::plausible(REG_DATA, data)
                || !Self::plausible(REG_DIR, dir)
            {
                return fx;
            }
            self.live = true;
        }
        self.data = (data & 0xF) as u8;
        self.dir = (dir & 0xF) as u8;
        self.pins_changed(now, &mut fx);
        fx
    }

    /// Re-evaluate the pins after `data` or `dir` changed.
    fn pins_changed(&mut self, now: u64, fx: &mut GpioEffect) {
        let was = self.driven;
        self.driven = self.data & self.dir;
        let (cs, sck) = (self.driven & CS != 0, self.driven & SCK != 0);
        let (was_cs, was_sck) = (was & CS != 0, was & SCK != 0);
        if cs && !was_cs {
            // CS rising opens the transfer. An SCK edge in the same update (the
            // standard driver sets DATA = SCK|CS first and only then switches the
            // direction register to all-output) is not a data clock.
            self.phase = Phase::Command { value: 0, bits: 0 };
            self.sio = false;
            return;
        } else if !cs && was_cs {
            self.phase = Phase::Idle;
            self.sio = false;
        }
        if !cs {
            return;
        }
        if sck && !was_sck {
            let bit = self.driven & SIO != 0;
            self.rising(bit, now, fx);
        } else if !sck && was_sck {
            self.falling();
        }
    }

    fn rising(&mut self, bit: bool, now: u64, fx: &mut GpioEffect) {
        match &mut self.phase {
            Phase::Command { value, bits } => {
                *value = *value << 1 | u8::from(bit);
                *bits += 1;
                if *bits == 8 {
                    let v = *value;
                    self.command(v, now, fx);
                }
            }
            Phase::Write {
                reg,
                len,
                buf,
                got,
                bits,
            } => {
                buf[usize::from(*got)] |= u8::from(bit) << *bits;
                *bits += 1;
                if *bits == 8 {
                    *bits = 0;
                    *got += 1;
                    if got == len {
                        let (reg, buf, len) = (*reg, *buf, usize::from(*len));
                        self.finish_write(reg, &buf[..len], now, fx);
                        self.phase = Phase::Idle;
                    }
                }
            }
            Phase::Idle | Phase::Read { .. } => {}
        }
    }

    fn falling(&mut self) {
        if let Phase::Read {
            buf,
            len,
            byte,
            bit,
        } = &mut self.phase
        {
            if byte < len {
                self.sio = buf[usize::from(*byte)] >> *bit & 1 != 0;
                *bit += 1;
                if *bit == 8 {
                    *bit = 0;
                    *byte += 1;
                }
            } else {
                self.sio = false;
            }
        }
    }

    /// A complete command byte arrived.
    fn command(&mut self, value: u8, now: u64, fx: &mut GpioEffect) {
        if value >> 4 != 0b0110 {
            self.phase = Phase::Idle;
            return;
        }
        let reg = value >> 1 & 7;
        let read = value & 1 != 0;
        if !self.used {
            // The first real use: the image gains its footer, so it is saved now.
            self.used = true;
            fx.rtc_changed = true;
        }
        self.rtc.advance_to(now);
        let len = match reg {
            0 | 6 => 0,
            2 => 7,
            3 => 3,
            _ => 1,
        };
        match reg {
            0 => {
                self.rtc.reset();
                fx.rtc_changed = true;
            }
            6 => fx.irq = true,
            _ => {}
        }
        if len == 0 {
            self.phase = Phase::Idle;
            return;
        }
        if !read {
            self.phase = Phase::Write {
                reg,
                len,
                buf: [0; 7],
                got: 0,
                bits: 0,
            };
            return;
        }
        let mut buf = [0xFF; 7];
        match reg {
            1 => {
                buf[0] = self.rtc.control;
                // The power-off flag clears when it is read.
                if self.rtc.control & CTRL_POWER != 0 {
                    self.rtc.control &= !CTRL_POWER;
                    fx.rtc_changed = true;
                }
            }
            2 => buf = self.rtc.read_datetime(),
            3 => buf[..3].copy_from_slice(&self.rtc.read_time()),
            _ => {}
        }
        self.phase = Phase::Read {
            buf,
            len,
            byte: 0,
            bit: 0,
        };
    }

    fn finish_write(&mut self, reg: u8, bytes: &[u8], now: u64, fx: &mut GpioEffect) {
        self.rtc.advance_to(now);
        match (reg, bytes) {
            (1, [c]) => self.rtc.write_control(*c),
            (2, [a, b, c, d, e, f, g]) => self.rtc.write_datetime([*a, *b, *c, *d, *e, *f, *g]),
            (3, [h, m, s]) => self.rtc.write_time([*h, *m, *s]),
            // Alarm and free registers: accepted, nothing stored.
            _ => return,
        }
        fx.rtc_changed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cart::rtc::RtcTime;

    /// A port with reads enabled and SCK, SIO, CS all outputs.
    fn port() -> Gpio {
        let mut g = Gpio::new(RtcMode::On);
        g.write16(REG_CTRL, 1, 0);
        g.write16(REG_DIR, 7, 0);
        g
    }

    fn start(g: &mut Gpio, now: u64) {
        g.write16(REG_DATA, SCK.into(), now);
        g.write16(REG_DATA, (SCK | CS).into(), now);
    }

    /// Clock one bit out: data and CS with SCK low, then SCK high.
    fn send_bit(g: &mut Gpio, bit: u8, now: u64) {
        g.write16(REG_DATA, u16::from(bit << 1 | CS), now);
        g.write16(REG_DATA, u16::from(bit << 1 | CS | SCK), now);
    }

    fn command(g: &mut Gpio, cmd: u8, read: bool, now: u64) -> GpioEffect {
        let byte = 0x60 | cmd << 1 | u8::from(read);
        let mut fx = GpioEffect::default();
        for i in (0..8).rev() {
            // `send_bit` ignores effects; run the last bit by hand to catch them.
            if i == 0 {
                g.write16(REG_DATA, u16::from((byte & 1) << 1 | CS), now);
                fx = g.write16(REG_DATA, u16::from((byte & 1) << 1 | CS | SCK), now);
            } else {
                send_bit(g, byte >> i & 1, now);
            }
        }
        fx
    }

    fn write_bytes(g: &mut Gpio, bytes: &[u8], now: u64) -> GpioEffect {
        let mut fx = GpioEffect::default();
        for (n, b) in bytes.iter().enumerate() {
            for i in 0..8 {
                let bit = b >> i & 1;
                if n == bytes.len() - 1 && i == 7 {
                    g.write16(REG_DATA, u16::from(bit << 1 | CS), now);
                    fx = g.write16(REG_DATA, u16::from(bit << 1 | CS | SCK), now);
                } else {
                    send_bit(g, bit, now);
                }
            }
        }
        fx
    }

    /// Switch SIO to an input and read `n` bytes.
    fn read_bytes(g: &mut Gpio, n: usize) -> Vec<u8> {
        g.write16(REG_DIR, 5, 0);
        (0..n)
            .map(|_| {
                (0..8).fold(0u8, |v, i| {
                    g.write16(REG_DATA, u16::from(CS), 0);
                    g.write16(REG_DATA, u16::from(CS | SCK), 0);
                    let sio = (g.read16(REG_DATA).unwrap() >> 1 & 1) as u8;
                    v | sio << i
                })
            })
            .collect()
    }

    fn end(g: &mut Gpio) {
        g.write16(REG_DATA, SCK.into(), 0);
        g.write16(REG_DIR, 7, 0);
    }

    fn set(g: &mut Gpio, t: RtcTime) {
        g.rtc_mut().set_time(t, 0);
        g.rtc_mut().control |= 0x40;
    }

    #[test]
    fn reads_the_status_register_and_the_power_flag_clears() {
        let mut g = port();
        start(&mut g, 0);
        command(&mut g, 1, true, 0);
        assert_eq!(
            read_bytes(&mut g, 1),
            [0x82],
            "power flag + bit 1 at power-on"
        );
        end(&mut g);
        start(&mut g, 0);
        command(&mut g, 1, true, 0);
        assert_eq!(read_bytes(&mut g, 1), [0x02], "the flag cleared on read");
    }

    #[test]
    fn reads_datetime_and_time_in_bcd() {
        let mut g = port();
        set(&mut g, RtcTime::new(2024, 3, 9, 23, 59, 58).unwrap());
        start(&mut g, 0);
        command(&mut g, 2, true, 0);
        assert_eq!(
            read_bytes(&mut g, 7),
            [0x24, 0x03, 0x09, 6, 0x23, 0x59, 0x58]
        );
        end(&mut g);
        start(&mut g, 0);
        command(&mut g, 3, true, 0);
        assert_eq!(read_bytes(&mut g, 3), [0x23, 0x59, 0x58]);
    }

    #[test]
    fn writes_the_status_then_datetime_and_time() {
        let mut g = port();
        start(&mut g, 0);
        let fx = command(&mut g, 1, false, 0);
        assert!(
            fx.rtc_changed,
            "the first command is the first use of the clock"
        );
        assert_eq!(g.rtc().control, 0x82, "but no register changed yet");
        let fx = write_bytes(&mut g, &[0x40], 0);
        assert!(fx.rtc_changed);
        assert_eq!(g.rtc().control, 0x80 | 0x40, "24h set, power flag kept");
        end(&mut g);

        start(&mut g, 0);
        command(&mut g, 2, false, 0);
        write_bytes(&mut g, &[0x25, 0x12, 0x31, 3, 0x23, 0x59, 0x00], 0);
        end(&mut g);
        let t = g.rtc().time();
        assert_eq!(
            (
                t.year(),
                t.month(),
                t.day(),
                t.weekday(),
                t.hour(),
                t.minute()
            ),
            (2025, 12, 31, 3, 23, 59)
        );

        start(&mut g, 0);
        command(&mut g, 3, false, 0);
        write_bytes(&mut g, &[0x01, 0x02, 0x03], 0);
        end(&mut g);
        let t = g.rtc().time();
        assert_eq!((t.day(), t.hour(), t.minute(), t.second()), (31, 1, 2, 3));
    }

    #[test]
    fn a_transfer_cut_short_by_cs_low_writes_nothing() {
        let mut g = port();
        set(&mut g, RtcTime::new(2024, 1, 1, 0, 0, 0).unwrap());
        start(&mut g, 0);
        command(&mut g, 3, false, 0);
        write_bytes(&mut g, &[0x11, 0x22], 0);
        end(&mut g);
        assert_eq!(g.rtc().time().hour(), 0);
        // The next transfer starts clean.
        start(&mut g, 0);
        command(&mut g, 3, false, 0);
        write_bytes(&mut g, &[0x05, 0x06, 0x07], 0);
        assert_eq!(g.rtc().time().second(), 7);
    }

    #[test]
    fn force_reset_clears_everything_but_day_and_month() {
        let mut g = port();
        set(&mut g, RtcTime::new(2024, 7, 14, 9, 8, 7).unwrap());
        start(&mut g, 0);
        let fx = command(&mut g, 0, false, 0);
        assert!(fx.rtc_changed && !fx.irq);
        let t = g.rtc().time();
        assert_eq!(
            (
                t.year(),
                t.month(),
                t.day(),
                t.weekday(),
                t.hour(),
                t.minute(),
                t.second()
            ),
            (2000, 1, 1, 0, 0, 0, 0)
        );
        assert_eq!(g.rtc().control, 0);
    }

    #[test]
    fn force_irq_requests_the_game_pak_interrupt() {
        let mut g = port();
        start(&mut g, 0);
        let fx = command(&mut g, 6, false, 0);
        assert!(fx.irq);
        end(&mut g);
        start(&mut g, 0);
        let fx = command(&mut g, 6, false, 0);
        assert!(
            fx.irq && !fx.rtc_changed,
            "only the first use bumps the save"
        );
    }

    #[test]
    fn alarm_and_free_registers_read_ff_and_accept_writes() {
        let mut g = port();
        for reg in [4, 5, 7] {
            start(&mut g, 0);
            command(&mut g, reg, true, 0);
            assert_eq!(read_bytes(&mut g, 1), [0xFF], "register {reg}");
            end(&mut g);
        }
        start(&mut g, 0);
        command(&mut g, 4, false, 0);
        let fx = write_bytes(&mut g, &[0x12], 0);
        assert!(!fx.rtc_changed);
    }

    #[test]
    fn a_bad_command_byte_is_ignored_until_cs_cycles() {
        let mut g = port();
        set(&mut g, RtcTime::new(2024, 1, 1, 5, 5, 5).unwrap());
        start(&mut g, 0);
        for bit in [1, 1, 1, 1, 0, 1, 0, 1] {
            send_bit(&mut g, bit, 0);
        }
        // Whatever follows is not interpreted.
        write_bytes(&mut g, &[0x99, 0x99, 0x99], 0);
        assert_eq!(g.rtc().time().hour(), 5);
    }

    #[test]
    fn the_clock_runs_on_emulated_cycles_between_commands() {
        let mut g = port();
        set(&mut g, RtcTime::new(2024, 1, 1, 0, 0, 0).unwrap());
        let later = 90 * u64::from(crate::CYCLES_PER_SECOND);
        start(&mut g, later);
        command(&mut g, 3, true, later);
        assert_eq!(read_bytes(&mut g, 3), [0x00, 0x01, 0x30]);
    }

    #[test]
    fn direction_masks_which_pins_the_gba_drives() {
        let mut g = Gpio::new(RtcMode::On);
        g.write16(REG_CTRL, 1, 0);
        g.write16(REG_DIR, 0b0101, 0);
        g.write16(REG_DATA, 0b1111, 0);
        // Only the output bits read back from the latch; SIO (input) is the chip's.
        assert_eq!(g.read16(REG_DATA), Some(0b0101));
        assert_eq!(g.read16(REG_DIR), Some(0b0101));
        // Bits 4-15 are not stored.
        g.write16(REG_DIR, 0xFFF3, 0);
        assert_eq!(g.read16(REG_DIR), Some(3));
        g.write16(REG_DATA, 0xFFFF, 0);
        assert_eq!(g.read16(REG_DATA), Some(3));
    }

    #[test]
    fn reads_fall_through_to_rom_until_enabled() {
        let mut g = Gpio::new(RtcMode::On);
        g.write16(REG_DIR, 7, 0);
        g.write16(REG_DATA, 5, 0);
        assert_eq!(g.read16(REG_DATA), None);
        assert_eq!(g.read16(REG_DIR), None);
        assert_eq!(g.read16(REG_CTRL), None);
        g.write16(REG_CTRL, 1, 0);
        assert_eq!(g.read16(REG_DATA), Some(5));
        assert_eq!(g.read16(REG_CTRL), Some(1));
        g.write16(REG_CTRL, 0, 0);
        assert_eq!(g.read16(REG_DATA), None, "disabling hides them again");
        g.write16(REG_CTRL, 1, 0);
        assert_eq!(g.read16(0xCA), None, "only the three registers");
    }

    #[test]
    fn auto_wakes_on_a_plausible_write_only() {
        let mut g = Gpio::new(RtcMode::Auto);
        g.write16(REG_CTRL, 1, 0);
        assert!(g.live(), "enabling reads is plausible");
        let mut g = Gpio::new(RtcMode::Auto);
        assert_eq!(g.read16(REG_CTRL), None, "inert before any write");
        g.write16(REG_DATA, 0x1234, 0);
        g.write16(REG_DIR, 0x0100, 0);
        g.write16(REG_CTRL, 2, 0);
        g.write16(0xCA, 1, 0);
        assert!(
            !g.live(),
            "implausible values and other addresses do nothing"
        );
        g.write16(REG_DATA, 1, 0);
        assert!(g.live());

        let mut off = Gpio::new(RtcMode::Off);
        off.write16(REG_DATA, 1, 0);
        off.write16(REG_CTRL, 1, 0);
        assert!(!off.live());
        assert_eq!(off.read16(REG_CTRL), None);
    }
}
