//! ARM-state instruction handlers and the 4096-entry decode table.
//!
//! The table is indexed by opcode bits 27..20 and 7..4 (the bits that tell the
//! instruction classes apart) and built at compile time by [`build`]. Handlers
//! that differ only in a few opcode bits are generic over those bits as a const
//! `F`, so the compiler specialises them (one data-processing function per
//! operation and S flag, and so on).

use super::alu::{add_with_carry, mul_cycles, shift_imm, shift_reg, sub_with_carry};
use super::{Cpu, FLAG_N, FLAG_Z, Handler, MODE_USR, PSR_T};
use crate::bus::{Access, CpuBus};

/// Expand to a match that instantiates `$f::<B, N>` for the literal N equal to
/// `$v`. The selector is always below the length of the list.
macro_rules! pick {
    ($f:ident, $v:expr, [$($n:literal)+]) => {
        match $v {
            $( $n => $f::<B, $n> as Handler<B>, )+
            _ => undefined::<B> as Handler<B>,
        }
    };
}
pub(super) use pick;

macro_rules! pick16 {
    ($f:ident, $v:expr) => {
        pick!($f, $v, [0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15])
    };
}
macro_rules! pick32 {
    ($f:ident, $v:expr) => {
        pick!($f, $v, [0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31])
    };
}
macro_rules! pick64 {
    ($f:ident, $v:expr) => {
        pick!($f, $v, [0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63])
    };
}
macro_rules! pick128 {
    ($f:ident, $v:expr) => {
        pick!($f, $v, [0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68 69 70 71 72 73 74 75 76 77 78 79 80 81 82 83 84 85 86 87 88 89 90 91 92 93 94 95 96 97 98 99 100 101 102 103 104 105 106 107 108 109 110 111 112 113 114 115 116 117 118 119 120 121 122 123 124 125 126 127])
    };
}
pub(super) use pick16;

/// Build the ARM decode table.
pub(super) const fn build<B: CpuBus>() -> [Handler<B>; 4096] {
    let mut t = [undefined::<B> as Handler<B>; 4096];
    let mut i = 0;
    while i < 4096 {
        t[i] = decode::<B>(i);
        i += 1;
    }
    t
}

/// `idx` = opcode bits 27..20 (high 8 bits) and 7..4 (low 4 bits).
const fn decode<B: CpuBus>(idx: usize) -> Handler<B> {
    let hi = idx >> 4;
    let lo = idx & 0xF;
    match hi >> 5 {
        0b000 => {
            if lo & 9 == 9 {
                // Bits 7 and 4 set: multiply, swap or halfword transfer.
                if lo & 6 == 0 {
                    if hi >> 2 == 0 {
                        pick!(mul, hi & 3, [0 1 2 3])
                    } else if hi >> 3 == 1 {
                        pick!(mul_long, hi & 7, [0 1 2 3 4 5 6 7])
                    } else if hi >> 3 == 2 && hi & 3 == 0 {
                        pick!(swp, (hi >> 2) & 1, [0 1])
                    } else {
                        undefined::<B>
                    }
                } else if hi & 1 == 0 && (lo >> 1) & 3 != 1 {
                    // A store with SH = 10 or 11 is LDRD/STRD (ARMv5): undefined.
                    undefined::<B>
                } else {
                    pick128!(halfword, ((hi & 0x1F) << 2) | ((lo >> 1) & 3))
                }
            } else if (hi >> 3) == 2 && hi & 1 == 0 {
                // TST/TEQ/CMP/CMN without S: PSR transfer and BX.
                match (hi, lo) {
                    (0x10 | 0x14, 0) => mrs::<B> as Handler<B>,
                    (0x12 | 0x16, 0) => msr_reg::<B> as Handler<B>,
                    (0x12, 1) => bx::<B> as Handler<B>,
                    _ => undefined::<B>,
                }
            } else if lo & 1 == 0 {
                pick32!(dp_shift_imm, hi & 0x1F)
            } else {
                pick32!(dp_shift_reg, hi & 0x1F)
            }
        }
        0b001 => {
            if (hi >> 3) & 3 == 2 && hi & 1 == 0 {
                // TST/TEQ/CMP/CMN without S: MSR immediate (or undefined).
                if hi & 2 != 0 {
                    msr_imm::<B>
                } else {
                    undefined::<B>
                }
            } else {
                pick32!(dp_imm, hi & 0x1F)
            }
        }
        0b010 => pick64!(single_transfer, hi & 0x3F),
        0b011 => {
            if lo & 1 == 1 {
                undefined::<B>
            } else {
                pick64!(single_transfer, hi & 0x3F)
            }
        }
        0b100 => pick32!(block_transfer, hi & 0x1F),
        0b101 => pick!(branch, (hi >> 4) & 1, [0 1]),
        0b110 => undefined::<B>,
        _ => {
            if hi & 0x10 != 0 {
                swi::<B>
            } else {
                undefined::<B>
            }
        }
    }
}

fn undefined<B: CpuBus>(cpu: &mut Cpu, bus: &mut B, _op: u32) {
    cpu.undefined(bus);
}

// ---- data processing -----------------------------------------------------

/// Shared tail of the data-processing handlers. `F` = opcode bits 24..20
/// (operation in bits 4:1, S in bit 0); `sc` is the shifter carry.
#[inline(always)]
fn dp_exec<B: CpuBus, const F: u32>(
    cpu: &mut Cpu,
    bus: &mut B,
    op: u32,
    rn: u32,
    op2: u32,
    sc: bool,
) {
    let opc = (F >> 1) & 0xF;
    let s = F & 1 != 0;
    let cin = u32::from(cpu.carry());
    let (res, carry, ovf, arith) = match opc {
        0x0 | 0x8 => (rn & op2, sc, false, false),
        0x1 | 0x9 => (rn ^ op2, sc, false, false),
        0x2 | 0xA => {
            let (r, c, v) = sub_with_carry(rn, op2, 1);
            (r, c, v, true)
        }
        0x3 => {
            let (r, c, v) = sub_with_carry(op2, rn, 1);
            (r, c, v, true)
        }
        0x4 | 0xB => {
            let (r, c, v) = add_with_carry(rn, op2, 0);
            (r, c, v, true)
        }
        0x5 => {
            let (r, c, v) = add_with_carry(rn, op2, cin);
            (r, c, v, true)
        }
        0x6 => {
            let (r, c, v) = sub_with_carry(rn, op2, cin);
            (r, c, v, true)
        }
        0x7 => {
            let (r, c, v) = sub_with_carry(op2, rn, cin);
            (r, c, v, true)
        }
        0xC => (rn | op2, sc, false, false),
        0xD => (op2, sc, false, false),
        0xE => (rn & !op2, sc, false, false),
        _ => (!op2, sc, false, false),
    };
    let rd = ((op >> 12) & 0xF) as usize;
    if (0x8..=0xB).contains(&opc) {
        // TSTP/TEQP/CMPP/CMNP (Rd=R15): CPSR = SPSR where there is one. The
        // result is not written to the PC, but if the restored CPSR changes the
        // state the pipeline holds the wrong opcode width: refill the next
        // instruction in the new state (as the Rd=R15 path below does).
        if rd == 15 && cpu.spsr_index().is_some() {
            let thumb = cpu.cpsr & PSR_T;
            cpu.restore_spsr();
            if cpu.cpsr & PSR_T != thumb {
                let next = cpu.r[15].wrapping_sub(4);
                cpu.refill(bus, next);
            }
        } else {
            cpu.set_nz(res);
            cpu.set_c(carry);
            if arith {
                cpu.set_v(ovf);
            }
        }
        return;
    }
    if s {
        if rd == 15 && cpu.spsr_index().is_some() {
            // GBATEK "ARM Opcodes: Data Processing": S with Rd=R15 restores CPSR.
            cpu.restore_spsr();
        } else {
            cpu.set_nz(res);
            cpu.set_c(carry);
            if arith {
                cpu.set_v(ovf);
            }
        }
    }
    if rd == 15 {
        cpu.refill(bus, res);
    } else {
        cpu.r[rd] = res;
    }
}

fn dp_imm<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let rot = ((op >> 8) & 0xF) * 2;
    let imm = (op & 0xFF).rotate_right(rot);
    let sc = if rot == 0 {
        cpu.carry()
    } else {
        imm >> 31 != 0
    };
    let rn = cpu.r[((op >> 16) & 0xF) as usize];
    dp_exec::<B, F>(cpu, bus, op, rn, imm, sc);
}

fn dp_shift_imm<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let rm = cpu.r[(op & 0xF) as usize];
    let (v, sc) = shift_imm((op >> 5) & 3, rm, (op >> 7) & 0x1F, cpu.carry());
    let rn = cpu.r[((op >> 16) & 0xF) as usize];
    dp_exec::<B, F>(cpu, bus, op, rn, v, sc);
}

/// Register-specified shift: one extra internal cycle, and PC reads as +12.
fn dp_shift_reg<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let rm_i = (op & 0xF) as usize;
    let rn_i = ((op >> 16) & 0xF) as usize;
    let amount = cpu.r[((op >> 8) & 0xF) as usize];
    let pc_adj = |i: usize, v: u32| if i == 15 { v.wrapping_add(4) } else { v };
    let rm = pc_adj(rm_i, cpu.r[rm_i]);
    let rn = pc_adj(rn_i, cpu.r[rn_i]);
    let (v, sc) = shift_reg((op >> 5) & 3, rm, amount, cpu.carry());
    cpu.internal(bus, 1);
    dp_exec::<B, F>(cpu, bus, op, rn, v, sc);
}

// ---- PSR transfer --------------------------------------------------------

fn mrs<B: CpuBus>(cpu: &mut Cpu, _bus: &mut B, op: u32) {
    let value = if op & (1 << 22) != 0 {
        // Reading the SPSR in user/system mode (which has none) gives the CPSR.
        cpu.spsr_index().map_or(cpu.cpsr, |b| cpu.spsr[b])
    } else {
        cpu.cpsr
    };
    cpu.r[((op >> 12) & 0xF) as usize] = value;
}

fn msr_write(cpu: &mut Cpu, op: u32, value: u32) {
    let mut mask = 0;
    if op & (1 << 19) != 0 {
        mask |= 0xFF00_0000;
    }
    if op & (1 << 18) != 0 {
        mask |= 0x00FF_0000;
    }
    if op & (1 << 17) != 0 {
        mask |= 0x0000_FF00;
    }
    if op & (1 << 16) != 0 {
        mask |= 0x0000_00FF;
    }
    if op & (1 << 22) != 0 {
        if let Some(b) = cpu.spsr_index() {
            cpu.spsr[b] = (cpu.spsr[b] & !mask) | (value & mask);
        }
    } else {
        if cpu.cpsr & 0x1F == MODE_USR {
            // User mode may only change the condition flags.
            mask &= 0xFF00_0000;
        }
        // Mode bit 4 is hard-wired to 1.
        let new = ((cpu.cpsr & !mask) | (value & mask)) | 0x10;
        cpu.set_cpsr(new);
    }
}

fn msr_reg<B: CpuBus>(cpu: &mut Cpu, _bus: &mut B, op: u32) {
    let v = cpu.r[(op & 0xF) as usize];
    msr_write(cpu, op, v);
}

fn msr_imm<B: CpuBus>(cpu: &mut Cpu, _bus: &mut B, op: u32) {
    let v = (op & 0xFF).rotate_right(((op >> 8) & 0xF) * 2);
    msr_write(cpu, op, v);
}

// ---- branches ------------------------------------------------------------

fn branch<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let offset = (((op & 0x00FF_FFFF) << 8) as i32 >> 6) as u32;
    if F != 0 {
        cpu.r[14] = cpu.r[15].wrapping_sub(4);
    }
    let target = cpu.r[15].wrapping_add(offset);
    cpu.refill(bus, target);
}

fn bx<B: CpuBus>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let target = cpu.r[(op & 0xF) as usize];
    cpu.cpsr = (cpu.cpsr & !PSR_T) | ((target & 1) << 5);
    cpu.refill(bus, target);
}

fn swi<B: CpuBus>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    cpu.software_interrupt(bus, op);
}

// ---- multiply ------------------------------------------------------------

/// MUL/MLA. `F` = A (bit 1), S (bit 0).
fn mul<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let rd = ((op >> 16) & 0xF) as usize;
    let rs = cpu.late_reg(((op >> 8) & 0xF) as usize);
    let mut res = cpu.late_reg((op & 0xF) as usize).wrapping_mul(rs);
    let mut cycles = mul_cycles(rs, true);
    if F & 2 != 0 {
        res = res.wrapping_add(cpu.late_reg(((op >> 12) & 0xF) as usize));
        cycles += 1;
    }
    cpu.internal(bus, cycles);
    cpu.write_reg(bus, rd, res);
    if F & 1 != 0 {
        // C is architecturally meaningless after a multiply; it is left alone
        // (the reference data computes some Booth-adder carry; see cpu_sst.rs).
        cpu.set_nz(res);
    }
}

/// UMULL/UMLAL/SMULL/SMLAL. `F` = signed (bit 2), A (bit 1), S (bit 0).
fn mul_long<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let hi = ((op >> 16) & 0xF) as usize;
    let lo = ((op >> 12) & 0xF) as usize;
    let rs = cpu.late_reg(((op >> 8) & 0xF) as usize);
    let rm = cpu.late_reg((op & 0xF) as usize);
    let signed = F & 4 != 0;
    let mut res = if signed {
        (i64::from(rm as i32) * i64::from(rs as i32)) as u64
    } else {
        u64::from(rm) * u64::from(rs)
    };
    let mut cycles = mul_cycles(rs, signed) + 1;
    if F & 2 != 0 {
        res = res.wrapping_add((u64::from(cpu.late_reg(hi)) << 32) | u64::from(cpu.late_reg(lo)));
        cycles += 1;
    }
    cpu.internal(bus, cycles);
    cpu.write_reg(bus, lo, res as u32);
    cpu.write_reg(bus, hi, (res >> 32) as u32);
    if F & 1 != 0 {
        cpu.cpsr = (cpu.cpsr & !(FLAG_N | FLAG_Z))
            | (((res >> 63) as u32) << 31)
            | (u32::from(res == 0) << 30);
    }
}

// ---- single data transfer ------------------------------------------------

/// Rotate a word loaded from a misaligned address (the bus aligns it down).
#[inline(always)]
fn rotate_load(value: u32, addr: u32) -> u32 {
    value.rotate_right((addr & 3) * 8)
}

/// LDR/STR/LDRB/STRB. `F` = opcode bits 25..20: I P U B W L.
fn single_transfer<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let rn_i = ((op >> 16) & 0xF) as usize;
    let rd_i = ((op >> 12) & 0xF) as usize;
    let offset = if F & 0x20 == 0 {
        op & 0xFFF
    } else {
        let rm = cpu.r[(op & 0xF) as usize];
        shift_imm((op >> 5) & 3, rm, (op >> 7) & 0x1F, cpu.carry()).0
    };
    let base = cpu.r[rn_i];
    let moved = if F & 8 != 0 {
        base.wrapping_add(offset)
    } else {
        base.wrapping_sub(offset)
    };
    let pre = F & 0x10 != 0;
    let addr = if pre { moved } else { base };
    let writeback = !pre || F & 2 != 0;
    if F & 1 != 0 {
        let value = if F & 4 != 0 {
            u32::from(bus.read8(addr, Access::NonSeq))
        } else {
            rotate_load(bus.read32(addr, Access::NonSeq), addr)
        };
        cpu.internal(bus, 1);
        cpu.seq = false;
        if writeback {
            cpu.write_reg(bus, rn_i, moved);
        }
        cpu.write_reg(bus, rd_i, value);
    } else {
        // A stored PC reads as instruction + 12.
        let value = if rd_i == 15 {
            cpu.r[15].wrapping_add(4)
        } else {
            cpu.r[rd_i]
        };
        if F & 4 != 0 {
            bus.write8(addr, value as u8, Access::NonSeq);
        } else {
            bus.write32(addr, value, Access::NonSeq);
        }
        cpu.seq = false;
        if writeback {
            cpu.write_reg(bus, rn_i, moved);
        }
    }
}

/// LDRH/STRH/LDRSB/LDRSH. `F` = P U I W L (bits 6..2) and SH (bits 1..0).
fn halfword<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let rn_i = ((op >> 16) & 0xF) as usize;
    let rd_i = ((op >> 12) & 0xF) as usize;
    let offset = if F & 0x10 != 0 {
        ((op >> 4) & 0xF0) | (op & 0xF)
    } else {
        cpu.r[(op & 0xF) as usize]
    };
    let base = cpu.r[rn_i];
    let moved = if F & 0x20 != 0 {
        base.wrapping_add(offset)
    } else {
        base.wrapping_sub(offset)
    };
    let pre = F & 0x40 != 0;
    let addr = if pre { moved } else { base };
    let writeback = !pre || F & 8 != 0;
    if F & 4 != 0 {
        let value = match F & 3 {
            // LDRH: a misaligned halfword comes back rotated by 8.
            1 => u32::from(bus.read16(addr, Access::NonSeq)).rotate_right((addr & 1) * 8),
            // LDRSB.
            2 => i32::from(bus.read8(addr, Access::NonSeq) as i8) as u32,
            // LDRSH: a misaligned one sign-extends the byte at the address.
            _ => {
                let h = bus.read16(addr, Access::NonSeq);
                if addr & 1 != 0 {
                    i32::from((h >> 8) as i8) as u32
                } else {
                    i32::from(h as i16) as u32
                }
            }
        };
        cpu.internal(bus, 1);
        cpu.seq = false;
        if writeback {
            cpu.write_reg(bus, rn_i, moved);
        }
        cpu.write_reg(bus, rd_i, value);
    } else {
        let value = if rd_i == 15 {
            cpu.r[15].wrapping_add(4)
        } else {
            cpu.r[rd_i]
        };
        bus.write16(addr, value as u16, Access::NonSeq);
        cpu.seq = false;
        if writeback {
            cpu.write_reg(bus, rn_i, moved);
        }
    }
}

// ---- block data transfer -------------------------------------------------

/// LDM/STM. `F` = opcode bits 24..20: P U S W L.
pub(super) fn block_transfer<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let rn_i = ((op >> 16) & 0xF) as usize;
    let mut list = op & 0xFFFF;
    let base = cpu.r[rn_i];
    let pre = F & 0x10 != 0;
    let up = F & 8 != 0;
    let s_bit = F & 4 != 0;
    let wb = F & 2 != 0;
    let load = F & 1 != 0;

    // An empty list transfers R15 only, but the base moves as if all 16 were.
    let count = if list == 0 {
        list = 1 << 15;
        16
    } else {
        list.count_ones()
    };
    let span = count * 4;
    let wb_value = if up {
        base.wrapping_add(span)
    } else {
        base.wrapping_sub(span)
    };
    // Transfers always run upward from the lowest address.
    let mut addr = match (up, pre) {
        (true, false) => base,
        (true, true) => base.wrapping_add(4),
        (false, false) => base.wrapping_sub(span).wrapping_add(4),
        (false, true) => base.wrapping_sub(span),
    };

    // S bit: LDM with R15 restores CPSR; otherwise the user bank is transferred.
    let user_bank = s_bit && !(load && list & (1 << 15) != 0);
    let mut access = Access::NonSeq;
    let mut first = true;
    let mut rest = list;

    if load {
        let mut pc_value = None;
        while rest != 0 {
            let i = rest.trailing_zeros() as usize;
            rest &= rest - 1;
            let value = bus.read32(addr, access);
            access = Access::Seq;
            addr = addr.wrapping_add(4);
            if first && wb {
                // The base is written back before the loads land, so a loaded
                // base register wins (ARMv4). With the S bit the transfer runs
                // with the user bank switched in, writeback included.
                if user_bank {
                    *cpu.user_reg(rn_i) = wb_value;
                } else {
                    cpu.write_reg(bus, rn_i, wb_value);
                }
            }
            first = false;
            if i == 15 {
                pc_value = Some(value);
            } else if user_bank {
                *cpu.user_reg(i) = value;
            } else {
                cpu.r[i] = value;
            }
        }
        cpu.internal(bus, 1);
        cpu.seq = false;
        if let Some(pc) = pc_value {
            if s_bit {
                cpu.restore_spsr();
            }
            cpu.refill(bus, pc);
        }
    } else {
        while rest != 0 {
            let i = rest.trailing_zeros() as usize;
            rest &= rest - 1;
            let value = if i == 15 {
                // A stored PC reads as instruction + 12 (ARM) or + 6 (Thumb).
                cpu.r[15].wrapping_add(if cpu.cpsr & PSR_T != 0 { 2 } else { 4 })
            } else if user_bank {
                *cpu.user_reg(i)
            } else {
                cpu.r[i]
            };
            bus.write32(addr, value, access);
            access = Access::Seq;
            addr = addr.wrapping_add(4);
            if first && wb {
                // Written back after the first store: a base that is not the
                // lowest register in the list is stored with its new value.
                if user_bank {
                    *cpu.user_reg(rn_i) = wb_value;
                } else {
                    cpu.write_reg(bus, rn_i, wb_value);
                }
            }
            first = false;
        }
        cpu.seq = false;
    }
}

// ---- swap ----------------------------------------------------------------

/// SWP/SWPB. `F` = B (byte).
fn swp<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let addr = cpu.late_reg(((op >> 16) & 0xF) as usize);
    let rd_i = ((op >> 12) & 0xF) as usize;
    let src = cpu.late_reg((op & 0xF) as usize);
    let value = if F != 0 {
        let v = u32::from(bus.read8(addr, Access::NonSeq));
        bus.write8(addr, src as u8, Access::NonSeq);
        v
    } else {
        let v = rotate_load(bus.read32(addr, Access::NonSeq), addr);
        bus.write32(addr, src, Access::NonSeq);
        v
    };
    cpu.internal(bus, 1);
    cpu.seq = false;
    cpu.write_reg(bus, rd_i, value);
}
