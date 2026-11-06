//! The ARM7TDMI (ARMv4T: ARM + Thumb). Owned by P1-CPU.
//!
//! **Contract:** [`Cpu::reset`], [`Cpu::post_bios`], [`Cpu::step`],
//! [`Cpu::registers`] and [`Registers`]. The CPU is generic over
//! [`CpuBus`](crate::bus::CpuBus), charges every access through it with the right
//! [`Access`](crate::bus::Access) kind, and checks `bus.irq_line()` (plus CPSR.I)
//! before each instruction. SWIs go to `bus.swi` first (HLE BIOS); on
//! `Unhandled` the CPU takes the exception.
//!
//! Layout: `mod.rs` holds the state, mode banking, exceptions and the step loop;
//! `arm.rs` and `thumb.rs` hold the instruction handlers and their decode tables
//! (DECISIONS #50); `alu.rs` the arithmetic helpers. Pipeline and N/S modelling
//! is DECISIONS #51.

mod alu;
mod arm;
mod thumb;

use crate::bus::{Access, CpuBus, SwiResult};
use crate::state::{StateError, check, get_u32s, put_u32s};
use emu_common::state::{StateReader, StateWriter};
use std::marker::PhantomData;

pub(crate) const FLAG_N: u32 = 1 << 31;
pub(crate) const FLAG_Z: u32 = 1 << 30;
pub(crate) const FLAG_C: u32 = 1 << 29;
pub(crate) const FLAG_V: u32 = 1 << 28;
pub(crate) const PSR_I: u32 = 1 << 7;
pub(crate) const PSR_T: u32 = 1 << 5;
const MODE_MASK: u32 = 0x1F;

const MODE_USR: u32 = 0x10;
const MODE_FIQ: u32 = 0x11;
const MODE_IRQ: u32 = 0x12;
const MODE_SVC: u32 = 0x13;
const MODE_ABT: u32 = 0x17;
const MODE_UND: u32 = 0x1B;

/// Register bank of each mode: 0 = user/system, then FIQ, IRQ, SVC, ABT, UND.
/// Invalid mode values (reachable through MSR) fall back to the user bank.
const BANK_OF_MODE: [u8; 32] = {
    let mut t = [0u8; 32];
    t[MODE_FIQ as usize] = 1;
    t[MODE_IRQ as usize] = 2;
    t[MODE_SVC as usize] = 3;
    t[MODE_ABT as usize] = 4;
    t[MODE_UND as usize] = 5;
    t
};

#[inline(always)]
fn bank_of(mode: u32) -> usize {
    BANK_OF_MODE[(mode & MODE_MASK) as usize] as usize
}

/// For each condition code, a bitmask indexed by the NZCV nibble: bit `nzcv` is
/// set when the condition passes. Code 0xF ("never" on ARMv4) is all zero.
const COND_PASS: [u16; 16] = {
    let mut t = [0u16; 16];
    let mut cond = 0;
    while cond < 15 {
        let mut nzcv = 0u32;
        while nzcv < 16 {
            let n = nzcv & 8 != 0;
            let z = nzcv & 4 != 0;
            let c = nzcv & 2 != 0;
            let v = nzcv & 1 != 0;
            let pass = match cond {
                0 => z,
                1 => !z,
                2 => c,
                3 => !c,
                4 => n,
                5 => !n,
                6 => v,
                7 => !v,
                8 => c && !z,
                9 => !c || z,
                10 => n == v,
                11 => n != v,
                12 => !z && n == v,
                13 => z || n != v,
                _ => true,
            };
            if pass {
                t[cond] |= 1 << nzcv;
            }
            nzcv += 1;
        }
        cond += 1;
    }
    t
};

/// A snapshot of the registers visible in the current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Registers {
    /// r0..=r15 as the current mode sees them. `r[15]` is the address of the next
    /// instruction to execute (not the pipelined +8/+4 value).
    pub r: [u32; 16],
    pub cpsr: u32,
}

/// The complete CPU state in the layout of the SingleStepTests ARM7TDMI files:
/// `r` is the user/system view (r8-r14 are the *user* registers even when a
/// banked mode is current) and each `r_*` array holds that mode's own banked
/// registers (`r_fiq` = r8..r14, the others r13, r14). `r[15]` is the raw
/// pipelined value (instruction address + 8 in ARM state, + 4 in Thumb).
/// Test hook; not part of the stable API.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CpuState {
    pub r: [u32; 16],
    pub r_fiq: [u32; 7],
    pub r_svc: [u32; 2],
    pub r_abt: [u32; 2],
    pub r_irq: [u32; 2],
    pub r_und: [u32; 2],
    pub cpsr: u32,
    /// SPSR of FIQ, SVC, ABT, IRQ, UND (in that order).
    pub spsr: [u32; 5],
    /// The two prefetched opcodes (next to execute first).
    pub pipeline: [u32; 2],
    /// Whether the next opcode fetch is sequential (S) rather than non-sequential (N).
    pub next_fetch_seq: bool,
}

#[derive(Clone)]
pub struct Cpu {
    /// The registers live in the current mode. `r[15]` is the address of the next
    /// opcode *fetch*: instruction address + 8 (ARM) or + 4 (Thumb) while an
    /// instruction executes, which is what the instruction reads as PC.
    r: [u32; 16],
    cpsr: u32,
    /// SPSR per bank (index 0, user/system, is unused).
    spsr: [u32; 6],
    /// r13/r14 of every bank that is not live. The live bank's slot is stale.
    banked: [[u32; 2]; 6],
    /// FIQ's r8-r12 while another mode is live, the user's while FIQ is live.
    fiq_hi: [u32; 5],
    usr_hi: [u32; 5],
    /// The 3-stage pipeline: the opcodes fetched but not executed yet.
    pipe: [u32; 2],
    /// The next opcode fetch is sequential to the last access (S) rather than N.
    seq: bool,
    /// Set by `refill`; tells `step` not to advance r15 past the instruction.
    flushed: bool,
    /// After reset/post_bios there is no bus yet: `r[15]` holds the raw start
    /// address and `step` fills the pipeline before executing.
    needs_refill: bool,
    /// A SWI that the HLE BIOS answered `WaitThenRetry`: its opcode, to run again
    /// on the next `step` with the pipeline exactly as it was while it executed.
    retry: Option<u32>,
}

type Handler<B> = fn(&mut Cpu, &mut B, u32);

struct Tables<B>(PhantomData<B>);

impl<B: CpuBus> Tables<B> {
    const ARM: [Handler<B>; 4096] = arm::build::<B>();
    const THUMB: [Handler<B>; 1024] = thumb::build::<B>();
}

impl Cpu {
    fn blank(cpsr: u32, pc: u32) -> Self {
        let mut r = [0; 16];
        r[15] = pc;
        Self {
            r,
            cpsr,
            spsr: [0; 6],
            banked: [[0; 2]; 6],
            fiq_hi: [0; 5],
            usr_hi: [0; 5],
            pipe: [0; 2],
            seq: false,
            flushed: false,
            needs_refill: true,
            retry: None,
        }
    }

    /// Power-on state: Supervisor mode, IRQ and FIQ masked, ARM state, PC = 0
    /// (boots a real BIOS).
    pub fn reset() -> Self {
        Self::blank(0xD3, 0)
    }

    /// The state the BIOS hands a cartridge: System mode, ARM state, PC =
    /// 0x0800_0000, SP_usr/sys = 0x0300_7F00, SP_irq = 0x0300_7FA0, SP_svc =
    /// 0x0300_7FE0 (GBATEK "BIOS RAM Usage"). Used with the HLE BIOS.
    pub fn post_bios() -> Self {
        let mut cpu = Self::blank(0x1F, 0x0800_0000);
        cpu.r[13] = 0x0300_7F00;
        cpu.banked[bank_of(MODE_IRQ)][0] = 0x0300_7FA0;
        cpu.banked[bank_of(MODE_SVC)][0] = 0x0300_7FE0;
        cpu
    }

    pub fn registers(&self) -> Registers {
        let mut r = self.r;
        if !self.needs_refill {
            // Undo the pipelined PC: report the next instruction to execute.
            r[15] = r[15].wrapping_sub(if self.cpsr & PSR_T != 0 { 4 } else { 8 });
        }
        Registers { r, cpsr: self.cpsr }
    }

    /// Whether an HLE wait is pending a retry of its SWI (test hook).
    #[cfg(test)]
    pub(crate) fn retrying(&self) -> bool {
        self.retry.is_some()
    }

    /// Load a complete state (test hook for the SingleStepTests harness).
    pub fn set_state(&mut self, s: &CpuState) {
        let mut c = Self::blank(s.cpsr, 0);
        c.needs_refill = false;
        c.r = s.r;
        c.banked[0] = [s.r[13], s.r[14]];
        c.usr_hi.copy_from_slice(&s.r[8..13]);
        c.fiq_hi.copy_from_slice(&s.r_fiq[..5]);
        c.banked[1] = [s.r_fiq[5], s.r_fiq[6]];
        c.banked[2] = s.r_irq;
        c.banked[3] = s.r_svc;
        c.banked[4] = s.r_abt;
        c.banked[5] = s.r_und;
        c.spsr = [0, s.spsr[0], s.spsr[3], s.spsr[1], s.spsr[2], s.spsr[4]];
        c.pipe = s.pipeline;
        c.seq = s.next_fetch_seq;
        // `r` holds the user view; make the current mode's registers live.
        c.cpsr = MODE_USR;
        c.switch_mode(s.cpsr & MODE_MASK);
        c.cpsr = s.cpsr;
        *self = c;
    }

    /// The complete state in the SingleStepTests layout (test hook).
    pub fn state(&self) -> CpuState {
        let mut c = self.clone();
        c.switch_mode(MODE_USR);
        let mut s = CpuState {
            r: c.r,
            cpsr: self.cpsr,
            pipeline: self.pipe,
            next_fetch_seq: self.seq,
            ..CpuState::default()
        };
        if self.needs_refill {
            s.r[15] = self.r[15];
        }
        s.r_fiq[..5].copy_from_slice(&c.fiq_hi);
        s.r_fiq[5..].copy_from_slice(&c.banked[1]);
        s.r_irq = c.banked[2];
        s.r_svc = c.banked[3];
        s.r_abt = c.banked[4];
        s.r_und = c.banked[5];
        s.spsr = [c.spsr[1], c.spsr[3], c.spsr[4], c.spsr[2], c.spsr[5]];
        s
    }

    // ---- save states ------------------------------------------------------

    /// Every field exactly as it is, including the banked slot of the live mode
    /// (stale, but part of the byte-for-byte snapshot) and the pipeline
    /// (DECISIONS #73).
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        put_u32s(w, &self.r);
        w.u32(self.cpsr);
        put_u32s(w, &self.spsr);
        for bank in &self.banked {
            put_u32s(w, bank);
        }
        put_u32s(w, &self.fiq_hi);
        put_u32s(w, &self.usr_hi);
        put_u32s(w, &self.pipe);
        w.bool(self.seq);
        w.bool(self.flushed);
        w.bool(self.needs_refill);
        w.bool(self.retry.is_some());
        w.u32(self.retry.unwrap_or(0));
    }

    /// The CPSR mode is not checked: MSR can write any value to an SPSR and
    /// `restore_spsr` copies it into the CPSR, and `bank_of` maps all 32 mode
    /// values (an invalid one to the user bank), so every value is reachable and
    /// harmless. Nor is the pipeline checked against the state bit: the Thumb
    /// decode masks its index, so no pipeline contents can fault.
    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let regs = get_u32s::<16>(r)?;
        let cpsr = r.u32()?;
        let spsr = get_u32s::<6>(r)?;
        let mut banked = [[0; 2]; 6];
        for bank in &mut banked {
            *bank = get_u32s::<2>(r)?;
        }
        let fiq_hi = get_u32s::<5>(r)?;
        let usr_hi = get_u32s::<5>(r)?;
        let pipe = get_u32s::<2>(r)?;
        let (seq, flushed, needs_refill) = (r.bool()?, r.bool()?, r.bool()?);
        let has_retry = r.bool()?;
        let retry_op = r.u32()?;
        check(has_retry || retry_op == 0, "retry opcode without a retry")?;
        *self = Self {
            r: regs,
            cpsr,
            spsr,
            banked,
            fiq_hi,
            usr_hi,
            pipe,
            seq,
            flushed,
            needs_refill,
            retry: has_retry.then_some(retry_op),
        };
        Ok(())
    }

    // ---- modes and PSRs ---------------------------------------------------

    /// Swap the banked registers so that `new_mode`'s are live. Does not touch
    /// `cpsr` (it must still hold the old mode, which is what gets saved).
    fn switch_mode(&mut self, new_mode: u32) {
        let old = bank_of(self.cpsr);
        let new = bank_of(new_mode);
        if old == new {
            return;
        }
        self.banked[old] = [self.r[13], self.r[14]];
        if old == 1 {
            self.fiq_hi.copy_from_slice(&self.r[8..13]);
            self.r[8..13].copy_from_slice(&self.usr_hi);
        }
        if new == 1 {
            self.usr_hi.copy_from_slice(&self.r[8..13]);
            self.r[8..13].copy_from_slice(&self.fiq_hi);
        }
        self.r[13] = self.banked[new][0];
        self.r[14] = self.banked[new][1];
    }

    /// Write the whole CPSR, switching register banks if the mode changes.
    #[inline]
    fn set_cpsr(&mut self, value: u32) {
        self.switch_mode(value);
        self.cpsr = value;
    }

    /// The SPSR of the current mode, if it has one (not user/system).
    #[inline]
    fn spsr_index(&self) -> Option<usize> {
        match bank_of(self.cpsr) {
            0 => None,
            b => Some(b),
        }
    }

    /// CPSR = SPSR (data processing / LDM with S set and PC as destination).
    /// User and system mode have no SPSR: nothing happens.
    fn restore_spsr(&mut self) {
        if let Some(b) = self.spsr_index() {
            let spsr = self.spsr[b];
            self.set_cpsr(spsr);
        }
    }

    /// Register `n` of the user bank (for LDM/STM with the S bit).
    fn user_reg(&mut self, n: usize) -> &mut u32 {
        let bank = bank_of(self.cpsr);
        match n {
            8..=12 if bank == 1 => &mut self.usr_hi[n - 8],
            13 | 14 if bank != 0 => &mut self.banked[0][n - 13],
            _ => &mut self.r[n],
        }
    }

    /// Read register `i` as an instruction with an internal cycle before the
    /// read sees it: r15 is already one word further (instruction + 12).
    #[inline(always)]
    fn late_reg(&self, i: usize) -> u32 {
        if i == 15 {
            self.r[15].wrapping_add(4)
        } else {
            self.r[i]
        }
    }

    #[inline(always)]
    fn set_nz(&mut self, v: u32) {
        self.cpsr = (self.cpsr & !(FLAG_N | FLAG_Z)) | (v & FLAG_N) | (u32::from(v == 0) << 30);
    }

    #[inline(always)]
    fn set_c(&mut self, c: bool) {
        self.cpsr = (self.cpsr & !FLAG_C) | (u32::from(c) << 29);
    }

    #[inline(always)]
    fn set_v(&mut self, v: bool) {
        self.cpsr = (self.cpsr & !FLAG_V) | (u32::from(v) << 28);
    }

    #[inline(always)]
    fn carry(&self) -> bool {
        self.cpsr & FLAG_C != 0
    }

    // ---- pipeline ---------------------------------------------------------

    #[inline(always)]
    fn fetch_access(&self) -> Access {
        if self.seq {
            Access::Seq
        } else {
            Access::NonSeq
        }
    }

    /// Refill the pipeline from `target` in the current state (ARM or Thumb): an
    /// N fetch of the target, an S fetch after it, and r15 ends up two opcodes
    /// ahead (GBATEK "ARM CPU Instruction Cycle Times": branches are 2S+1N).
    fn refill<B: CpuBus>(&mut self, bus: &mut B, target: u32) {
        if self.cpsr & PSR_T == 0 {
            // Fetches are word aligned, but r15 keeps the low bits of the target
            // (the reference data, NanoBoyAdvance, does the same; DECISIONS #51).
            let t = target & !3;
            self.pipe[0] = bus.fetch32(t, Access::NonSeq);
            self.pipe[1] = bus.fetch32(t.wrapping_add(4), Access::Seq);
            self.r[15] = target.wrapping_add(8);
        } else {
            let t = target & !1;
            self.pipe[0] = u32::from(bus.fetch16(t, Access::NonSeq));
            self.pipe[1] = u32::from(bus.fetch16(t.wrapping_add(2), Access::Seq));
            self.r[15] = t.wrapping_add(4);
        }
        self.seq = true;
        self.flushed = true;
    }

    /// Charge `n` internal (I) cycles. The bus is idle for them, so the next
    /// opcode fetch cannot be sequential.
    #[inline(always)]
    fn internal<B: CpuBus>(&mut self, bus: &mut B, n: u32) {
        bus.idle(n);
        self.seq = false;
    }

    /// Write register `i`; a write to r15 is a branch and refills the pipeline.
    #[inline(always)]
    fn write_reg<B: CpuBus>(&mut self, bus: &mut B, i: usize, value: u32) {
        if i == 15 {
            self.refill(bus, value);
        } else {
            self.r[i] = value;
        }
    }

    // ---- exceptions -------------------------------------------------------

    /// Enter an exception: bank to `mode`, save CPSR in its SPSR, `lr` into r14,
    /// ARM state with IRQs masked, and refill from `vector` (GBATEK "ARM CPU
    /// Exceptions").
    fn enter_exception<B: CpuBus>(&mut self, bus: &mut B, mode: u32, vector: u32, lr: u32) {
        let old = self.cpsr;
        self.switch_mode(mode);
        self.spsr[bank_of(mode)] = old;
        self.r[14] = lr;
        self.cpsr = (old & !(MODE_MASK | PSR_T)) | mode | PSR_I;
        self.refill(bus, vector);
    }

    /// Return address of an SWI/undefined exception raised by the instruction
    /// being executed: the address of the next instruction.
    #[inline(always)]
    fn next_instruction_lr(&self) -> u32 {
        self.r[15].wrapping_sub(if self.cpsr & PSR_T != 0 { 2 } else { 4 })
    }

    /// The undefined-instruction exception (vector 0x04).
    fn undefined<B: CpuBus>(&mut self, bus: &mut B) {
        let lr = self.next_instruction_lr();
        self.enter_exception(bus, MODE_UND, 0x04, lr);
    }

    /// The SWI in `op` (ARM: comment in bits 16..=23; Thumb: bits 0..=7): the HLE
    /// BIOS gets the first go.
    ///
    /// `Done` costs nothing beyond the SWI's own fetch (the HLE charges what its
    /// work would have taken). `WaitThenRetry` leaves the CPU *on* the SWI
    /// without fetching or refilling anything: the pipeline and r15 stay as they
    /// were mid-instruction and the next `step` runs the same opcode again
    /// (`registers().r[15]` reports the SWI's address meanwhile). Waiting
    /// therefore costs no bus accesses at all, only what the HLE charges per
    /// attempt. An IRQ taken instead of the retry returns to the SWI.
    /// `Unhandled` takes the SVC exception (2S + 1N like a branch).
    fn software_interrupt<B: CpuBus>(&mut self, bus: &mut B, op: u32) {
        let number = if self.cpsr & PSR_T != 0 {
            op as u8
        } else {
            (op >> 16) as u8
        };
        let mut regs = [self.r[0], self.r[1], self.r[2], self.r[3]];
        match bus.swi(number, &mut regs) {
            SwiResult::Done => self.r[..4].copy_from_slice(&regs),
            SwiResult::WaitThenRetry => {
                self.r[..4].copy_from_slice(&regs);
                self.retry = Some(op);
                // Keep r15: the retry runs in this same pipeline state.
                self.flushed = true;
            }
            SwiResult::Unhandled => {
                let lr = self.next_instruction_lr();
                self.enter_exception(bus, MODE_SVC, 0x08, lr);
            }
        }
    }

    /// Take the IRQ exception at an instruction boundary (vector 0x18). LR is the
    /// next instruction + 4 so the handler returns with `subs pc, lr, #4`.
    fn irq<B: CpuBus>(&mut self, bus: &mut B) {
        let thumb = self.cpsr & PSR_T != 0;
        // The prefetch that would have gone with the dropped instruction.
        let acc = self.fetch_access();
        if thumb {
            bus.fetch16(self.r[15], acc);
        } else {
            bus.fetch32(self.r[15], acc);
        }
        let lr = if thumb {
            self.r[15]
        } else {
            self.r[15].wrapping_sub(4)
        };
        self.enter_exception(bus, MODE_IRQ, 0x18, lr);
        self.flushed = false;
    }

    // ---- run loop ---------------------------------------------------------

    /// Execute one instruction (or take a pending IRQ).
    #[inline]
    pub fn step<B: CpuBus>(&mut self, bus: &mut B) {
        if self.needs_refill {
            self.needs_refill = false;
            let pc = self.r[15];
            self.refill(bus, pc);
            self.flushed = false;
        }
        if bus.irq_line() && self.cpsr & PSR_I == 0 {
            self.retry = None;
            self.irq(bus);
        } else if let Some(op) = self.retry.take() {
            self.software_interrupt(bus, op);
            if self.flushed {
                self.flushed = false;
            } else {
                let size = if self.cpsr & PSR_T != 0 { 2 } else { 4 };
                self.r[15] = self.r[15].wrapping_add(size);
            }
        } else if self.cpsr & PSR_T == 0 {
            self.step_arm(bus);
        } else {
            self.step_thumb(bus);
        }
    }

    #[inline(always)]
    fn step_arm<B: CpuBus>(&mut self, bus: &mut B) {
        let op = self.pipe[0];
        self.pipe[0] = self.pipe[1];
        let acc = self.fetch_access();
        self.pipe[1] = bus.fetch32(self.r[15], acc);
        self.seq = true;
        if COND_PASS[(op >> 28) as usize] >> (self.cpsr >> 28) & 1 != 0 {
            let idx = ((op >> 16) & 0xFF0) | ((op >> 4) & 0xF);
            let table = &Tables::<B>::ARM;
            table[idx as usize](self, bus, op);
        }
        if self.flushed {
            self.flushed = false;
        } else {
            self.r[15] = self.r[15].wrapping_add(4);
        }
    }

    #[inline(always)]
    fn step_thumb<B: CpuBus>(&mut self, bus: &mut B) {
        // Thumb opcodes are 16 bits. The pipeline holds ARM words only after an MSR
        // flipped the state bit without a refill (UNPREDICTABLE on hardware, kept
        // as the reference does); masking makes the decode safe for any contents.
        let op = self.pipe[0] & 0xFFFF;
        self.pipe[0] = self.pipe[1];
        let acc = self.fetch_access();
        self.pipe[1] = u32::from(bus.fetch16(self.r[15], acc));
        self.seq = true;
        let table = &Tables::<B>::THUMB;
        table[(op >> 6) as usize](self, bus, op);
        if self.flushed {
            self.flushed = false;
        } else {
            self.r[15] = self.r[15].wrapping_add(2);
        }
    }
}

#[cfg(test)]
mod tests;
