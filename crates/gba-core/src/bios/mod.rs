//! The BIOS: either the built-in high-level emulation (the default, DECISIONS #47,
//! #64, #65) or a user-supplied 16 KiB dump.
//!
//! **Contract:** [`Bios`], [`Bios::read8`], and [`swi`], which the bus's
//! `CpuBus::swi` forwards to.
//!
//! With [`Bios::Hle`], most SWIs finish in Rust ([`swi`] returns `Done` or
//! `WaitThenRetry`). SoftReset and HardReset change PC and the banked registers,
//! which `swi` cannot reach (it sees r0..=r3), so they return `Unhandled` and
//! the CPU takes the SWI exception into the built-in image ([`image`]), whose
//! code does the reset. The same path returns from SWIs the HLE does not know.
//! With [`Bios::Real`] every SWI is `Unhandled` and the dump's own code runs.
//!
//! References: GBATEK "BIOS Functions" (the per-call pages cited below) and
//! "BIOS RAM Usage".

pub mod image;
pub mod math;
mod memops;

use crate::bus::{Access, Bus, CpuBus, SwiResult, Width};

pub enum Bios {
    /// HLE: SWIs are emulated in Rust; the BIOS region holds the built-in image
    /// ([`image::IMAGE`]) instead of Nintendo's code.
    Hle,
    /// A real BIOS dump (exactly 16 KiB, validated by the host).
    Real(Box<[u8]>),
}

impl Bios {
    /// BIOS byte at `addr` (0..0x4000), with no read protection (the bus applies
    /// that: DECISIONS #52).
    pub fn read8(&self, addr: u32) -> u8 {
        match self {
            Bios::Hle => image::byte(addr),
            Bios::Real(data) => data.get(addr as usize).copied().unwrap_or(0),
        }
    }
}

// ---- timing model (DECISIONS #65) ---------------------------------------------
//
// Memory traffic is charged by the bus, access by access, with the widths the
// real BIOS uses. On top of that every call pays `SWI_OVERHEAD` (exception
// entry, dispatch table and return in BIOS code; paid again on every retry of a
// waiting call so a wait loop always advances time) and the compute cost below.

/// Exception entry, dispatch and return.
const SWI_OVERHEAD: u64 = 48;
const DIV_COST: u64 = 40;
/// GBATEK: DivArm is "slightly slower (3 clock cycles)" than Div.
const DIV_ARM_EXTRA: u64 = 3;
const SQRT_COST: u64 = 56;
const ARCTAN_COST: u64 = 40;
const ARCTAN2_COST: u64 = 60;
/// Summing the 4096 words of the BIOS.
const CHECKSUM_COST: u64 = 4096 * 4;
/// Stepping SOUNDBIAS by one level with a delay of 8, 0x200 times.
const SOUND_BIAS_COST: u64 = 0x200 * 16;
const MIDI_COST: u64 = 80;
const RAM_RESET_BASE: u64 = 100;

/// IWRAM word of the BIOS interrupt flags (GBATEK "BIOS RAM Usage": IRQ IF Check
/// Flags for IntrWait/VBlankIntrWait), written by the user's IRQ handler.
const BIOS_IF: u32 = 0x0300_7FF8;
/// SoftReset's re-entry flag byte.
const RESET_FLAG: u32 = 0x0300_7FFA;
const HALTCNT: u32 = 0x300;
const SOUNDBIAS: u32 = 0x88;

/// The state the BIOS leaves behind when the cartridge starts (DECISIONS #47,
/// #64). Called by `Gba::new` for [`Bios::Hle`]; charges no time. The CPU part
/// is `Cpu::post_bios`; the PPU already starts with BG2/BG3 PA and PD at 0x100.
///
/// * POSTFLG = 1 ("the BIOS has run").
/// * SOUNDBIAS = 0x200 (the bias level `SoundBias` and the reset use).
/// * IWRAM 0x03007E00..=0x03007FFF zero (the area SoftReset clears: stacks, the
///   user IRQ pointer at 0x03007FFC, the IntrWait flags at 0x03007FF8).
pub fn post_boot(bus: &mut Bus) {
    bus.iwram[0x7E00..].fill(0);
    bus.postflg = 1;
    bus.write_io16(SOUNDBIAS, 0x200, 0xFFFF);
}

/// Handle SWI `number` with r0..=r3 in `r`. A real BIOS runs its own code.
pub fn swi(bus: &mut Bus, number: u8, r: &mut [u32; 4]) -> SwiResult {
    if matches!(bus.bios, Bios::Real(_)) {
        return SwiResult::Unhandled;
    }
    let res = hle(bus, number, r);
    if res == SwiResult::Done {
        // The last BIOS opcode fetched was the SWI return path's (image.rs).
        bus.bios_latch = image::LATCH_AFTER_SWI;
    }
    res
}

fn hle(bus: &mut Bus, number: u8, r: &mut [u32; 4]) -> SwiResult {
    bus.sched.advance(SWI_OVERHEAD);
    match number {
        // SoftReset: the image's code resets the CPU and jumps (see module docs).
        0x00 => return SwiResult::Unhandled,
        0x01 => register_ram_reset(bus, r[0]),
        0x02 => halt(bus, 0x00),
        0x03 => halt(bus, 0x80),
        0x04 => return intr_wait(bus, r),
        0x05 => {
            // GBATEK: sets r0 = 1, r1 = 1 and runs IntrWait.
            r[0] = 1;
            r[1] = 1;
            return intr_wait(bus, r);
        }
        0x06 | 0x07 => {
            let (n, d) = if number == 0x06 {
                (r[0], r[1])
            } else {
                (r[1], r[0])
            };
            match math::div(n as i32, d as i32) {
                Some((q, rem, abs)) => {
                    (r[0], r[1], r[3]) = (q as u32, rem as u32, abs);
                    let extra = if number == 0x07 { DIV_ARM_EXTRA } else { 0 };
                    bus.sched.advance(DIV_COST + extra);
                }
                // GBATEK: "gets caught in an endless loop upon division by zero".
                // Halting between attempts keeps the loop interruptible by IRQs
                // and lets time pass without spinning the host.
                None => return hang(bus),
            }
        }
        0x08 => {
            r[0] = math::sqrt(r[0]);
            bus.sched.advance(SQRT_COST);
        }
        0x09 => {
            r[0] = math::arctan(i32::from(r[0] as i16)) as u32;
            bus.sched.advance(ARCTAN_COST);
        }
        0x0A => {
            r[0] = math::arctan2(i32::from(r[0] as i16), i32::from(r[1] as i16));
            bus.sched.advance(ARCTAN2_COST);
        }
        0x0B => memops::cpu_set(bus, r),
        0x0C => memops::cpu_fast_set(bus, r),
        0x0D => {
            r[0] = 0xBAAE_187F;
            bus.sched.advance(CHECKSUM_COST);
        }
        0x0E => memops::bg_affine_set(bus, r),
        0x0F => memops::obj_affine_set(bus, r),
        0x10 => memops::bit_unpack(bus, r),
        0x11 => memops::lz77(bus, r, false),
        0x12 => memops::lz77(bus, r, true),
        0x13 => memops::huff(bus, r),
        0x14 => memops::rl(bus, r, false),
        0x15 => memops::rl(bus, r, true),
        0x16 => memops::diff_unfilter(bus, r, false, false),
        0x17 => memops::diff_unfilter(bus, r, false, true),
        0x18 => memops::diff_unfilter(bus, r, true, false),
        0x19 => sound_bias(bus, r[0]),
        // The sound driver (SoundDriverInit/Mode/Main/VSync, SoundChannelClear,
        // the undocumented SoundWhatever0..4, VSyncOff/On, SoundGetJumpList):
        // documented no-ops. Games that use them also ship the driver code or
        // mix in software; the core does no I/O, so nothing is logged.
        0x1A..=0x1E | 0x20..=0x24 | 0x28..=0x2A => {}
        0x1F => {
            let freq = bus.read32(r[0].wrapping_add(4), Access::NonSeq);
            r[0] = math::midi_key_to_freq(freq, r[1] as u8, r[2] as u8);
            bus.sched.advance(MIDI_COST);
        }
        // MultiBoot needs a link partner; report "failed".
        0x25 => r[0] = 1,
        0x26 => {
            // HardReset: wipe everything, forget the soft-reset flag, then let
            // the image reboot the cartridge (no logo, no 2 second wait).
            register_ram_reset(bus, 0xFF);
            bus.write_value(RESET_FLAG, 0, Width::B);
            return SwiResult::Unhandled;
        }
        0x27 => halt(bus, r[2] as u8),
        // 0x2B.. jump to garbage on hardware; the image just returns.
        _ => return SwiResult::Unhandled,
    }
    SwiResult::Done
}

/// Write HALTCNT: 0x00 halts, 0x80 stops (SWI Halt, Stop, CustomHalt). The wake
/// check inside the write ends the halt at once if an enabled IRQ is pending.
fn halt(bus: &mut Bus, value: u8) {
    bus.write_io16(HALTCNT, u16::from(value) << 8, 0xFF00);
}

/// The division-by-zero hang: halt, and run the SWI again after the wake-up.
fn hang(bus: &mut Bus) -> SwiResult {
    bus.halted = true;
    bus.stopped = false;
    SwiResult::WaitThenRetry
}

/// SWI 0x04 IntrWait (GBATEK "BIOS Halt Functions"). The CPU re-runs the SWI with
/// the registers handed back, and an IRQ taken during the wait also returns to
/// the SWI, so the first attempt does the one-time work (IME = 1, discarding old
/// flags) and clears r0 so the retries only check.
fn intr_wait(bus: &mut Bus, r: &mut [u32; 4]) -> SwiResult {
    let wanted = r[1] as u16;
    bus.irq.ime = true;
    if r[0] != 0 {
        let flags = bus.read16(BIOS_IF, Access::NonSeq);
        bus.write16(BIOS_IF, flags & !wanted, Access::NonSeq);
        r[0] = 0;
    }
    let flags = bus.read16(BIOS_IF, Access::NonSeq);
    let hit = flags & wanted;
    if hit != 0 {
        // The matched flags are consumed.
        bus.write16(BIOS_IF, flags & !hit, Access::NonSeq);
        return SwiResult::Done;
    }
    bus.halted = true;
    bus.stopped = false;
    bus.check_wake();
    SwiResult::WaitThenRetry
}

/// SWI 0x19 SoundBias: SOUNDBIAS bits 0..=9 become 0 or 0x200, the rest stay.
fn sound_bias(bus: &mut Bus, level: u32) {
    let cur = bus.read_io16(SOUNDBIAS);
    let target = if level == 0 { 0 } else { 0x200 };
    bus.write_io16(SOUNDBIAS, cur & 0xFC00 | target, 0xFFFF);
    bus.sched.advance(SOUND_BIAS_COST);
}

/// SWI 0x01 RegisterRamReset (GBATEK "BIOS Reset Functions"). The per-register
/// values of the "all other registers" reset are not in GBATEK: video, DMA,
/// timers, KEYCNT, IE, IF and IME are zeroed, BG2/BG3 PA and PD become 0x100
/// (the identity), and DISPCNT ends as 0x0080 (forced blank) regardless of
/// `flags`, which GBATEK documents. WAITCNT and POSTFLG are left alone.
fn register_ram_reset(bus: &mut Bus, flags: u32) {
    bus.sched.advance(RAM_RESET_BASE);
    let cost =
        |bus: &Bus, base: u32, bytes: usize| (bytes as u64 / 4) * bus.cost(base, Width::W, Access::Seq);
    if flags & 0x01 != 0 {
        bus.ewram.fill(0);
        let c = cost(bus, 0x0200_0000, bus.ewram.len());
        bus.sched.advance(c);
    }
    if flags & 0x02 != 0 {
        // The last 0x200 bytes (stacks, IRQ pointer, flags) survive.
        bus.iwram[..0x7E00].fill(0);
        let c = cost(bus, 0x0300_0000, 0x7E00);
        bus.sched.advance(c);
    }
    if flags & 0x04 != 0 {
        bus.ppu.palette.fill(0);
        let c = cost(bus, 0x0500_0000, 0x400);
        bus.sched.advance(c);
    }
    if flags & 0x08 != 0 {
        bus.ppu.vram.fill(0);
        let c = cost(bus, 0x0600_0000, 0x1_8000);
        bus.sched.advance(c);
    }
    if flags & 0x10 != 0 {
        bus.ppu.oam.fill(0);
        let c = cost(bus, 0x0700_0000, 0x400);
        bus.sched.advance(c);
    }
    if flags & 0x20 != 0 {
        // SIO: general purpose mode (RCNT bit 15), everything else zero.
        for off in (0x120..0x160).step_by(2) {
            bus.write_io16(off, 0, 0xFFFF);
        }
        bus.write_io16(0x134, 0x8000, 0xFFFF);
    }
    if flags & 0x40 != 0 {
        // Master off first, so the PSG registers clear with it.
        bus.write_io16(0x84, 0, 0xFFFF);
        for off in (0x60..0x84).step_by(2) {
            bus.write_io16(off, 0, 0xFFFF);
        }
        bus.write_io16(SOUNDBIAS, 0x200, 0xFFFF);
    }
    if flags & 0x80 != 0 {
        let zero = |bus: &mut Bus, range: std::ops::Range<u32>| {
            for off in range.step_by(2) {
                bus.write_io16(off, 0, 0xFFFF);
            }
        };
        zero(bus, 0x000..0x060); // video
        zero(bus, 0x0B0..0x0E0); // DMA
        zero(bus, 0x100..0x110); // timers
        zero(bus, 0x132..0x134); // KEYCNT
        bus.write_io16(0x200, 0, 0xFFFF); // IE
        bus.write_io16(0x202, 0xFFFF, 0xFFFF); // acknowledge every flag
        bus.write_io16(0x208, 0, 0xFFFF); // IME
        for off in [0x20, 0x26, 0x30, 0x36] {
            bus.write_io16(off, 0x100, 0xFFFF); // BG2/BG3 PA and PD
        }
    }
    // GBATEK: always forced blank.
    bus.write_io16(0x000, 0x0080, 0xFFFF);
}
