//! Sharp SM83 CPU (Pan Docs: "CPU Registers and Flags", "CPU Instruction Set",
//! "Interrupts", "halt").
//!
//! # Contract (frozen; propose changes to the orchestrator)
//!
//! * [`Cpu::step`] executes exactly one of: an interrupt dispatch, one halted
//!   M-cycle, or one instruction. All cycle cost is paid through the
//!   [`CpuBus`] (`read`/`write`/`tick`, one M-cycle each).
//! * Opcode `0x40` (`LD B,B`) sets a flag readable with
//!   [`Cpu::take_ld_b_b_breakpoint`] (mooneye/acid2 test signal).
//! * Illegal opcodes (0xD3, 0xDB, ...) lock the CPU: [`Cpu::is_locked`] becomes
//!   true and subsequent steps only tick the bus.
//!
//! # Implementation notes
//!
//! * Decoding follows the `xx yyy zzz` bit-field layout of the opcode map
//!   (gbdev "Opcode Table" / Pan Docs "CPU Instruction Set"): `x = op >> 6`,
//!   `y = (op >> 3) & 7`, `z = op & 7`. Register operands are decoded by index
//!   (`B C D E H L (HL) A`), ALU operations by index (`ADD ADC SUB SBC AND XOR
//!   OR CP`), so there are no per-opcode arms for the regular blocks.
//! * Every instruction is written as the exact sequence of bus calls it makes,
//!   so memory accesses land on the right M-cycle (mooneye `*_timing` tests).
//! * `STOP` is simplified: it calls [`CpuBus::stop`], consumes its padding byte
//!   without a bus access and idles until a selected joypad line is low
//!   (Pan Docs "Joypad Input": wakes from STOP regardless of IE). Interrupts
//!   do not wake it, as on hardware where the clocks stop. Unlike hardware,
//!   the LCD/timer keep running during STOP.

use crate::bus::CpuBus;
use crate::interrupts::Interrupt;

#[cfg(test)]
mod tests;

const FLAG_Z: u8 = 0x80;
const FLAG_N: u8 = 0x40;
const FLAG_H: u8 = 0x20;
const FLAG_C: u8 = 0x10;

/// SM83 register file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Registers {
    pub a: u8,
    /// Flags: Z=0x80, N=0x40, H=0x20, C=0x10; low nibble always 0.
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    pub sp: u16,
    pub pc: u16,
}

impl Registers {
    /// DMG register state after the boot ROM (Pan Docs: "Power Up Sequence").
    pub const fn dmg_post_boot() -> Self {
        Self {
            a: 0x01,
            f: 0xB0,
            b: 0x00,
            c: 0x13,
            d: 0x00,
            e: 0xD8,
            h: 0x01,
            l: 0x4D,
            sp: 0xFFFE,
            pc: 0x0100,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Cpu {
    pub regs: Registers,
    /// Interrupt master enable.
    ime: bool,
    /// `EI` executed; IME turns on after the *next* instruction completes.
    ei_pending: bool,
    /// HALTed: idle until an interrupt is pending.
    halted: bool,
    /// STOPped (see module docs): idle until a selected joypad line is low.
    stopped: bool,
    /// HALT bug: the next opcode fetch does not increment PC.
    halt_bug: bool,
    locked: bool,
    ld_b_b: bool,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    pub fn new() -> Self {
        Self {
            regs: Registers::dmg_post_boot(),
            ime: false,
            ei_pending: false,
            halted: false,
            stopped: false,
            halt_bug: false,
            locked: false,
            ld_b_b: false,
        }
    }

    pub fn registers(&self) -> &Registers {
        &self.regs
    }

    /// True after an illegal opcode hung the CPU.
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Returns true (once) if `LD B,B` executed since the last call.
    pub fn take_ld_b_b_breakpoint(&mut self) -> bool {
        std::mem::take(&mut self.ld_b_b)
    }

    /// Interrupt master enable (for debuggers/tests).
    pub fn ime(&self) -> bool {
        self.ime
    }

    /// True while halted/stopped.
    pub fn is_halted(&self) -> bool {
        self.halted || self.stopped
    }

    pub fn step<B: CpuBus>(&mut self, bus: &mut B) {
        if self.locked {
            bus.tick();
            return;
        }
        if self.stopped {
            if !bus.joypad_line_low() {
                bus.tick();
                return;
            }
            self.stopped = false;
        }
        if self.halted {
            // Pan Docs "halt": wakes when IE & IF != 0, regardless of IME.
            if bus.pending_interrupts() == 0 {
                bus.tick();
                return;
            }
            self.halted = false;
        }
        if self.ime && bus.pending_interrupts() != 0 {
            self.dispatch_interrupt(bus);
            return;
        }
        // EI takes effect after the instruction that follows it. A DI inside
        // that window clears `ei_pending`, cancelling the enable.
        let enable_ime = self.ei_pending;
        let op = self.fetch_opcode(bus);
        self.execute(bus, op);
        if enable_ime && self.ei_pending {
            self.ei_pending = false;
            self.ime = true;
        }
    }

    // ---- Interrupt dispatch ----

    /// 5 M-cycles: 2 internal, push PCh, push PCl, set PC (Pan Docs
    /// "Interrupts"). IE is re-sampled after the high-byte push: if that write
    /// cancelled every pending interrupt the jump goes to 0x0000 (mooneye
    /// `acceptance/interrupts/ie_push`).
    fn dispatch_interrupt<B: CpuBus>(&mut self, bus: &mut B) {
        self.ime = false;
        self.ei_pending = false;
        bus.tick();
        bus.tick();
        let mut pc = self.regs.pc;
        if self.halt_bug {
            // `EI; HALT` with an interrupt pending (Pan Docs "halt bug"): the
            // interrupt is serviced and returns to the HALT, which then runs
            // again. The skipped-increment fetch never happens.
            self.halt_bug = false;
            pc = pc.wrapping_sub(1);
        }
        self.regs.sp = self.regs.sp.wrapping_sub(1);
        bus.write(self.regs.sp, (pc >> 8) as u8);
        let chosen = Interrupt::highest_priority(bus.pending_interrupts());
        self.regs.sp = self.regs.sp.wrapping_sub(1);
        bus.write(self.regs.sp, pc as u8);
        let vector = match chosen {
            Some(i) => {
                bus.acknowledge_interrupt(i.mask());
                i.vector()
            }
            None => 0x0000,
        };
        bus.tick();
        self.regs.pc = vector;
    }

    // ---- Fetch helpers ----

    fn fetch_opcode<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let op = bus.read(self.regs.pc);
        // HALT bug (Pan Docs "halt"): the byte after HALT is read twice.
        if self.halt_bug {
            self.halt_bug = false;
        } else {
            self.regs.pc = self.regs.pc.wrapping_add(1);
        }
        op
    }

    fn imm8<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let v = bus.read(self.regs.pc);
        self.regs.pc = self.regs.pc.wrapping_add(1);
        v
    }

    fn imm16<B: CpuBus>(&mut self, bus: &mut B) -> u16 {
        let lo = self.imm8(bus) as u16;
        let hi = self.imm8(bus) as u16;
        (hi << 8) | lo
    }

    /// Push without the leading internal cycle (callers add it where needed).
    fn push16<B: CpuBus>(&mut self, bus: &mut B, v: u16) {
        self.regs.sp = self.regs.sp.wrapping_sub(1);
        bus.write(self.regs.sp, (v >> 8) as u8);
        self.regs.sp = self.regs.sp.wrapping_sub(1);
        bus.write(self.regs.sp, v as u8);
    }

    fn pop16<B: CpuBus>(&mut self, bus: &mut B) -> u16 {
        let lo = bus.read(self.regs.sp) as u16;
        self.regs.sp = self.regs.sp.wrapping_add(1);
        let hi = bus.read(self.regs.sp) as u16;
        self.regs.sp = self.regs.sp.wrapping_add(1);
        (hi << 8) | lo
    }

    // ---- Register access by opcode field index ----

    fn af(&self) -> u16 {
        u16::from_be_bytes([self.regs.a, self.regs.f])
    }

    fn hl(&self) -> u16 {
        u16::from_be_bytes([self.regs.h, self.regs.l])
    }

    fn set_hl(&mut self, v: u16) {
        [self.regs.h, self.regs.l] = v.to_be_bytes();
    }

    /// 8-bit operand `B C D E H L (HL) A`; index 6 costs one bus read.
    fn get_r8<B: CpuBus>(&mut self, bus: &mut B, i: u8) -> u8 {
        let r = &self.regs;
        match i {
            0 => r.b,
            1 => r.c,
            2 => r.d,
            3 => r.e,
            4 => r.h,
            5 => r.l,
            6 => bus.read(self.hl()),
            _ => r.a,
        }
    }

    /// Index 6 costs one bus write.
    fn set_r8<B: CpuBus>(&mut self, bus: &mut B, i: u8, v: u8) {
        match i {
            0 => self.regs.b = v,
            1 => self.regs.c = v,
            2 => self.regs.d = v,
            3 => self.regs.e = v,
            4 => self.regs.h = v,
            5 => self.regs.l = v,
            6 => bus.write(self.hl(), v),
            _ => self.regs.a = v,
        }
    }

    /// 16-bit operand `BC DE HL SP`.
    fn get_r16(&self, i: u8) -> u16 {
        let r = &self.regs;
        match i {
            0 => u16::from_be_bytes([r.b, r.c]),
            1 => u16::from_be_bytes([r.d, r.e]),
            2 => self.hl(),
            _ => r.sp,
        }
    }

    fn set_r16(&mut self, i: u8, v: u16) {
        let [hi, lo] = v.to_be_bytes();
        match i {
            0 => (self.regs.b, self.regs.c) = (hi, lo),
            1 => (self.regs.d, self.regs.e) = (hi, lo),
            2 => (self.regs.h, self.regs.l) = (hi, lo),
            _ => self.regs.sp = v,
        }
    }

    /// PUSH/POP operand `BC DE HL AF` (index 3 is AF, not SP).
    fn get_r16_stack(&self, i: u8) -> u16 {
        if i == 3 { self.af() } else { self.get_r16(i) }
    }

    fn set_r16_stack(&mut self, i: u8, v: u16) {
        if i == 3 {
            // F's low nibble is hard-wired to 0.
            [self.regs.a, self.regs.f] = (v & 0xFFF0).to_be_bytes();
        } else {
            self.set_r16(i, v);
        }
    }

    /// Branch condition `NZ Z NC C`.
    fn cond(&self, i: u8) -> bool {
        match i & 3 {
            0 => self.regs.f & FLAG_Z == 0,
            1 => self.regs.f & FLAG_Z != 0,
            2 => self.regs.f & FLAG_C == 0,
            _ => self.regs.f & FLAG_C != 0,
        }
    }

    fn set_flags(&mut self, z: bool, n: bool, h: bool, c: bool) {
        self.regs.f = (u8::from(z) * FLAG_Z)
            | (u8::from(n) * FLAG_N)
            | (u8::from(h) * FLAG_H)
            | (u8::from(c) * FLAG_C);
    }

    // ---- Decode / execute ----

    fn execute<B: CpuBus>(&mut self, bus: &mut B, op: u8) {
        let y = (op >> 3) & 7;
        let z = op & 7;
        match op >> 6 {
            0 => self.exec_block0(bus, y, z),
            1 => {
                if op == 0x76 {
                    self.halt(bus);
                } else {
                    if op == 0x40 {
                        self.ld_b_b = true;
                    }
                    let v = self.get_r8(bus, z);
                    self.set_r8(bus, y, v);
                }
            }
            2 => {
                let v = self.get_r8(bus, z);
                self.alu(y, v);
            }
            _ => self.exec_block3(bus, y, z),
        }
    }

    /// HALT (Pan Docs "halt"). With IME=0 and an interrupt already pending the
    /// CPU does not halt and the next byte is read twice (HALT bug).
    fn halt<B: CpuBus>(&mut self, bus: &mut B) {
        if !self.ime && bus.pending_interrupts() != 0 {
            self.halt_bug = true;
        } else {
            self.halted = true;
        }
    }

    fn exec_block0<B: CpuBus>(&mut self, bus: &mut B, y: u8, z: u8) {
        match z {
            0 => match y {
                0 => {} // NOP
                1 => {
                    // LD (a16),SP
                    let addr = self.imm16(bus);
                    let sp = self.regs.sp;
                    bus.write(addr, sp as u8);
                    bus.write(addr.wrapping_add(1), (sp >> 8) as u8);
                }
                2 => {
                    // STOP (simplified, see module docs): skip padding byte.
                    self.regs.pc = self.regs.pc.wrapping_add(1);
                    bus.stop();
                    self.stopped = true;
                }
                3 => {
                    // JR e
                    let e = self.imm8(bus) as i8;
                    bus.tick();
                    self.regs.pc = self.regs.pc.wrapping_add_signed(e as i16);
                }
                _ => {
                    // JR cc,e: internal cycle only when taken.
                    let e = self.imm8(bus) as i8;
                    if self.cond(y - 4) {
                        bus.tick();
                        self.regs.pc = self.regs.pc.wrapping_add_signed(e as i16);
                    }
                }
            },
            1 => {
                let rr = y >> 1;
                if y & 1 == 0 {
                    let v = self.imm16(bus);
                    self.set_r16(rr, v);
                } else {
                    // ADD HL,rr: Z unchanged, H from bit 11, C from bit 15.
                    bus.tick();
                    let hl = self.hl();
                    let v = self.get_r16(rr);
                    let (res, c) = hl.overflowing_add(v);
                    let h = (hl & 0x0FFF) + (v & 0x0FFF) > 0x0FFF;
                    let z = self.regs.f & FLAG_Z != 0;
                    self.set_flags(z, false, h, c);
                    self.set_hl(res);
                }
            }
            2 => {
                // LD (rr),A / LD A,(rr) with rr = BC, DE, HL+, HL-.
                let rr = y >> 1;
                let addr = match rr {
                    0 | 1 => self.get_r16(rr),
                    2 => {
                        let hl = self.hl();
                        self.set_hl(hl.wrapping_add(1));
                        hl
                    }
                    _ => {
                        let hl = self.hl();
                        self.set_hl(hl.wrapping_sub(1));
                        hl
                    }
                };
                if y & 1 == 0 {
                    bus.write(addr, self.regs.a);
                } else {
                    self.regs.a = bus.read(addr);
                }
            }
            3 => {
                // INC rr / DEC rr: one internal cycle, no flags.
                bus.tick();
                let rr = y >> 1;
                let v = self.get_r16(rr);
                let v = if y & 1 == 0 {
                    v.wrapping_add(1)
                } else {
                    v.wrapping_sub(1)
                };
                self.set_r16(rr, v);
            }
            4 => {
                // INC r (C unchanged)
                let v = self.get_r8(bus, y);
                let r = v.wrapping_add(1);
                let c = self.regs.f & FLAG_C != 0;
                self.set_flags(r == 0, false, (v & 0x0F) + 1 > 0x0F, c);
                self.set_r8(bus, y, r);
            }
            5 => {
                // DEC r (C unchanged)
                let v = self.get_r8(bus, y);
                let r = v.wrapping_sub(1);
                let c = self.regs.f & FLAG_C != 0;
                self.set_flags(r == 0, true, v & 0x0F == 0, c);
                self.set_r8(bus, y, r);
            }
            6 => {
                let v = self.imm8(bus);
                self.set_r8(bus, y, v);
            }
            _ => self.accumulator_op(y),
        }
    }

    /// `RLCA RRCA RLA RRA DAA CPL SCF CCF`.
    fn accumulator_op(&mut self, y: u8) {
        match y {
            0..=3 => {
                // Same rotates as the CB group but Z is always cleared.
                self.regs.a = self.rotate_shift(y, self.regs.a);
                self.regs.f &= !FLAG_Z;
            }
            4 => self.daa(),
            5 => {
                self.regs.a = !self.regs.a;
                self.regs.f |= FLAG_N | FLAG_H;
            }
            6 => {
                let z = self.regs.f & FLAG_Z != 0;
                self.set_flags(z, false, false, true);
            }
            _ => {
                let z = self.regs.f & FLAG_Z != 0;
                let c = self.regs.f & FLAG_C != 0;
                self.set_flags(z, false, false, !c);
            }
        }
    }

    /// Decimal adjust A (gbdev "DAA").
    fn daa(&mut self) {
        let f = self.regs.f;
        let (n, h, mut c) = (f & FLAG_N != 0, f & FLAG_H != 0, f & FLAG_C != 0);
        let mut a = self.regs.a;
        if n {
            if c {
                a = a.wrapping_sub(0x60);
            }
            if h {
                a = a.wrapping_sub(0x06);
            }
        } else {
            if c || a > 0x99 {
                a = a.wrapping_add(0x60);
                c = true;
            }
            if h || a & 0x0F > 0x09 {
                a = a.wrapping_add(0x06);
            }
        }
        self.regs.a = a;
        self.set_flags(a == 0, n, false, c);
    }

    /// 8-bit ALU on A by index: `ADD ADC SUB SBC AND XOR OR CP`.
    fn alu(&mut self, op: u8, v: u8) {
        let a = self.regs.a;
        let carry = u8::from(self.regs.f & FLAG_C != 0);
        match op {
            0 => self.add8(a, v, 0, true),
            1 => self.add8(a, v, carry, true),
            2 => self.sub8(a, v, 0, true),
            3 => self.sub8(a, v, carry, true),
            4 => {
                self.regs.a = a & v;
                self.set_flags(self.regs.a == 0, false, true, false);
            }
            5 => {
                self.regs.a = a ^ v;
                self.set_flags(self.regs.a == 0, false, false, false);
            }
            6 => {
                self.regs.a = a | v;
                self.set_flags(self.regs.a == 0, false, false, false);
            }
            _ => self.sub8(a, v, 0, false), // CP: SUB without storing
        }
    }

    fn add8(&mut self, a: u8, v: u8, carry: u8, store: bool) {
        let r = a as u16 + v as u16 + carry as u16;
        let h = (a & 0x0F) + (v & 0x0F) + carry > 0x0F;
        if store {
            self.regs.a = r as u8;
        }
        self.set_flags(r as u8 == 0, false, h, r > 0xFF);
    }

    fn sub8(&mut self, a: u8, v: u8, carry: u8, store: bool) {
        let r = (a as u16).wrapping_sub(v as u16).wrapping_sub(carry as u16);
        let h = (a & 0x0F) < (v & 0x0F) + carry;
        let c = (a as u16) < v as u16 + carry as u16;
        if store {
            self.regs.a = r as u8;
        }
        self.set_flags(r as u8 == 0, true, h, c);
    }

    /// CB-group rotate/shift by index `RLC RRC RL RR SLA SRA SWAP SRL`; sets
    /// Z from the result.
    fn rotate_shift(&mut self, op: u8, v: u8) -> u8 {
        let cin = self.regs.f & FLAG_C != 0;
        let (r, c) = match op {
            0 => (v.rotate_left(1), v & 0x80 != 0),
            1 => (v.rotate_right(1), v & 0x01 != 0),
            2 => ((v << 1) | u8::from(cin), v & 0x80 != 0),
            3 => ((v >> 1) | (u8::from(cin) << 7), v & 0x01 != 0),
            4 => (v << 1, v & 0x80 != 0),
            5 => ((v >> 1) | (v & 0x80), v & 0x01 != 0),
            6 => (v.rotate_left(4), false),
            _ => (v >> 1, v & 0x01 != 0),
        };
        self.set_flags(r == 0, false, false, c);
        r
    }

    fn exec_block3<B: CpuBus>(&mut self, bus: &mut B, y: u8, z: u8) {
        match z {
            0 => match y {
                0..=3 => {
                    // RET cc: internal cycle for the condition check, then
                    // (taken) pop + internal cycle.
                    bus.tick();
                    if self.cond(y) {
                        self.regs.pc = self.pop16(bus);
                        bus.tick();
                    }
                }
                4 => {
                    let a8 = self.imm8(bus);
                    bus.write(0xFF00 | a8 as u16, self.regs.a);
                }
                5 => {
                    // ADD SP,e: flags from the low byte (Pan Docs "ADD SP, e8").
                    let e = self.imm8(bus);
                    bus.tick();
                    bus.tick();
                    self.regs.sp = self.sp_plus_e(e);
                }
                6 => {
                    let a8 = self.imm8(bus);
                    self.regs.a = bus.read(0xFF00 | a8 as u16);
                }
                _ => {
                    // LD HL,SP+e
                    let e = self.imm8(bus);
                    bus.tick();
                    let v = self.sp_plus_e(e);
                    self.set_hl(v);
                }
            },
            1 => {
                if y & 1 == 0 {
                    let v = self.pop16(bus);
                    self.set_r16_stack(y >> 1, v);
                } else {
                    match y >> 1 {
                        0 | 1 => {
                            // RET / RETI
                            self.regs.pc = self.pop16(bus);
                            bus.tick();
                            if y == 3 {
                                self.ime = true;
                            }
                        }
                        2 => self.regs.pc = self.hl(), // JP HL
                        _ => {
                            // LD SP,HL
                            bus.tick();
                            self.regs.sp = self.hl();
                        }
                    }
                }
            }
            2 => match y {
                0..=3 => {
                    // JP cc,a16
                    let addr = self.imm16(bus);
                    if self.cond(y) {
                        bus.tick();
                        self.regs.pc = addr;
                    }
                }
                4 => bus.write(0xFF00 | self.regs.c as u16, self.regs.a),
                5 => {
                    let addr = self.imm16(bus);
                    bus.write(addr, self.regs.a);
                }
                6 => self.regs.a = bus.read(0xFF00 | self.regs.c as u16),
                _ => {
                    let addr = self.imm16(bus);
                    self.regs.a = bus.read(addr);
                }
            },
            3 => match y {
                0 => {
                    // JP a16
                    let addr = self.imm16(bus);
                    bus.tick();
                    self.regs.pc = addr;
                }
                1 => {
                    let cb = self.imm8(bus);
                    self.exec_cb(bus, cb);
                }
                6 => {
                    // DI: also cancels a pending EI.
                    self.ime = false;
                    self.ei_pending = false;
                }
                7 => self.ei_pending = true, // EI
                _ => self.locked = true,     // D3 DB E3 EB
            },
            4 => {
                if y < 4 {
                    // CALL cc,a16
                    let addr = self.imm16(bus);
                    if self.cond(y) {
                        bus.tick();
                        let pc = self.regs.pc;
                        self.push16(bus, pc);
                        self.regs.pc = addr;
                    }
                } else {
                    self.locked = true; // E4 EC F4 FC
                }
            }
            5 => {
                if y & 1 == 0 {
                    // PUSH rr
                    bus.tick();
                    let v = self.get_r16_stack(y >> 1);
                    self.push16(bus, v);
                } else if y == 1 {
                    // CALL a16
                    let addr = self.imm16(bus);
                    bus.tick();
                    let pc = self.regs.pc;
                    self.push16(bus, pc);
                    self.regs.pc = addr;
                } else {
                    self.locked = true; // DD ED FD
                }
            }
            6 => {
                let v = self.imm8(bus);
                self.alu(y, v);
            }
            _ => {
                // RST y*8
                bus.tick();
                let pc = self.regs.pc;
                self.push16(bus, pc);
                self.regs.pc = u16::from(y) * 8;
            }
        }
    }

    /// `SP + signed e`; H and C come from the unsigned low-byte addition, Z=N=0.
    fn sp_plus_e(&mut self, e: u8) -> u16 {
        let sp = self.regs.sp;
        let h = (sp & 0x0F) + (e as u16 & 0x0F) > 0x0F;
        let c = (sp & 0xFF) + e as u16 > 0xFF;
        self.set_flags(false, false, h, c);
        sp.wrapping_add_signed(e as i8 as i16)
    }

    /// CB-prefixed opcode: `x=0` rotate/shift, `1` BIT, `2` RES, `3` SET.
    fn exec_cb<B: CpuBus>(&mut self, bus: &mut B, op: u8) {
        let y = (op >> 3) & 7;
        let z = op & 7;
        let v = self.get_r8(bus, z);
        match op >> 6 {
            0 => {
                let r = self.rotate_shift(y, v);
                self.set_r8(bus, z, r);
            }
            1 => {
                // BIT y,r: (HL) form is read-only (3 M-cycles total).
                let c = self.regs.f & FLAG_C != 0;
                self.set_flags(v & (1 << y) == 0, false, true, c);
            }
            2 => self.set_r8(bus, z, v & !(1 << y)),
            _ => self.set_r8(bus, z, v | (1 << y)),
        }
    }
}
