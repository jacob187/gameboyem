//! Thumb-state instruction handlers and the 1024-entry decode table, indexed by
//! opcode bits 15..6 (GBATEK "THUMB Instruction Set"). Handlers receive the
//! 16-bit opcode widened to `u32`; in Thumb state r15 reads as instruction + 4.

use super::alu::{add_with_carry, mul_cycles, shift_imm, shift_reg, sub_with_carry};
use super::arm::{block_transfer, pick, pick16};
use super::{COND_PASS, Cpu, Handler, PSR_T};
use crate::bus::{Access, CpuBus};

pub(super) const fn build<B: CpuBus>() -> [Handler<B>; 1024] {
    let mut t = [undefined::<B> as Handler<B>; 1024];
    let mut i = 0;
    while i < 1024 {
        t[i] = decode::<B>(i);
        i += 1;
    }
    t
}

/// `idx` = opcode bits 15..6.
const fn decode<B: CpuBus>(idx: usize) -> Handler<B> {
    match idx >> 7 {
        0b000 => {
            let kind = (idx >> 5) & 3;
            if kind == 3 {
                pick!(add_sub, (idx >> 3) & 3, [0 1 2 3])
            } else {
                pick!(shift_imm_op, kind, [0 1 2])
            }
        }
        0b001 => pick!(imm8_op, (idx >> 5) & 3, [0 1 2 3]),
        0b010 => {
            if idx >> 4 == 0b010000 {
                pick16!(alu_op, idx & 0xF)
            } else if idx >> 4 == 0b010001 {
                pick!(hi_reg, (idx >> 2) & 3, [0 1 2 3])
            } else if idx >> 5 == 0b01001 {
                ldr_pc::<B>
            } else {
                pick!(ls_reg, (idx >> 3) & 7, [0 1 2 3 4 5 6 7])
            }
        }
        0b011 => pick!(ls_imm, (idx >> 5) & 3, [0 1 2 3]),
        0b100 => {
            if (idx >> 6) & 1 == 0 {
                pick!(ls_half_imm, (idx >> 5) & 1, [0 1])
            } else {
                pick!(ls_sp, (idx >> 5) & 1, [0 1])
            }
        }
        0b101 => {
            if (idx >> 6) & 1 == 0 {
                pick!(add_pc_sp, (idx >> 5) & 1, [0 1])
            } else if idx >> 2 == 0b1011_0000 {
                pick!(add_sp, (idx >> 1) & 1, [0 1])
            } else if (idx >> 3) & 3 == 2 {
                pick!(push_pop, ((idx >> 4) & 2) | ((idx >> 2) & 1), [0 1 2 3])
            } else {
                undefined::<B>
            }
        }
        0b110 => {
            if (idx >> 6) & 1 == 0 {
                pick!(ldm_stm, (idx >> 5) & 1, [0 1])
            } else {
                pick16!(bcc, (idx >> 2) & 0xF)
            }
        }
        _ => match idx >> 5 {
            0b11100 => branch::<B>,
            0b11110 => bl_prefix::<B>,
            0b11111 => bl_suffix::<B>,
            _ => undefined::<B>,
        },
    }
}

fn undefined<B: CpuBus>(cpu: &mut Cpu, bus: &mut B, _op: u32) {
    cpu.undefined(bus);
}

#[inline(always)]
fn lo(op: u32, shift: u32) -> usize {
    ((op >> shift) & 7) as usize
}

/// Format 1: LSL/LSR/ASR by immediate. `F` = 0, 1, 2.
fn shift_imm_op<B: CpuBus, const F: u32>(cpu: &mut Cpu, _bus: &mut B, op: u32) {
    let (v, c) = shift_imm(F, cpu.r[lo(op, 3)], (op >> 6) & 0x1F, cpu.carry());
    cpu.r[lo(op, 0)] = v;
    cpu.set_nz(v);
    cpu.set_c(c);
}

/// Format 2: ADD/SUB with a register or 3-bit immediate. `F` = I (bit 1), SUB (bit 0).
fn add_sub<B: CpuBus, const F: u32>(cpu: &mut Cpu, _bus: &mut B, op: u32) {
    let a = cpu.r[lo(op, 3)];
    let b = if F & 2 != 0 {
        (op >> 6) & 7
    } else {
        cpu.r[lo(op, 6)]
    };
    let (r, c, v) = if F & 1 != 0 {
        sub_with_carry(a, b, 1)
    } else {
        add_with_carry(a, b, 0)
    };
    cpu.r[lo(op, 0)] = r;
    cpu.set_nz(r);
    cpu.set_c(c);
    cpu.set_v(v);
}

/// Format 3: MOV/CMP/ADD/SUB with an 8-bit immediate. `F` = 0..=3.
fn imm8_op<B: CpuBus, const F: u32>(cpu: &mut Cpu, _bus: &mut B, op: u32) {
    let rd = lo(op, 8);
    let imm = op & 0xFF;
    let a = cpu.r[rd];
    match F {
        0 => {
            cpu.r[rd] = imm;
            cpu.set_nz(imm);
        }
        1 | 3 => {
            let (r, c, v) = sub_with_carry(a, imm, 1);
            if F == 3 {
                cpu.r[rd] = r;
            }
            cpu.set_nz(r);
            cpu.set_c(c);
            cpu.set_v(v);
        }
        _ => {
            let (r, c, v) = add_with_carry(a, imm, 0);
            cpu.r[rd] = r;
            cpu.set_nz(r);
            cpu.set_c(c);
            cpu.set_v(v);
        }
    }
}

/// Format 4: ALU operations. `F` = opcode bits 9..6.
fn alu_op<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let rd = lo(op, 0);
    let a = cpu.r[rd];
    let b = cpu.r[lo(op, 3)];
    let cin = u32::from(cpu.carry());
    let res = match F {
        0 | 8 => a & b,
        1 => a ^ b,
        2 | 3 | 4 | 7 => {
            let kind = match F {
                2 => 0,
                3 => 1,
                4 => 2,
                _ => 3,
            };
            let (v, c) = shift_reg(kind, a, b, cpu.carry());
            cpu.internal(bus, 1);
            cpu.set_c(c);
            v
        }
        5 | 6 | 9 | 10 | 11 => {
            let (r, c, v) = match F {
                5 => add_with_carry(a, b, cin),
                6 => sub_with_carry(a, b, cin),
                9 => sub_with_carry(0, b, 1),
                10 => sub_with_carry(a, b, 1),
                _ => add_with_carry(a, b, 0),
            };
            cpu.set_c(c);
            cpu.set_v(v);
            r
        }
        12 => a | b,
        13 => {
            // MUL Rd,Rs is ARM `MULS Rd,Rs,Rd`: the multiplier is the old Rd.
            cpu.internal(bus, mul_cycles(a, true));
            a.wrapping_mul(b)
        }
        14 => a & !b,
        _ => !b,
    };
    cpu.set_nz(res);
    if !matches!(F, 8 | 10 | 11) {
        cpu.r[rd] = res;
    }
}

/// Format 5: ADD/CMP/MOV on high registers, and BX. `F` = 0..=3.
fn hi_reg<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let rs = ((op >> 3) & 0xF) as usize;
    let rd = ((op & 7) | ((op >> 4) & 8)) as usize;
    let b = cpu.r[rs];
    match F {
        0 | 2 => {
            let res = if F == 0 { cpu.r[rd].wrapping_add(b) } else { b };
            if rd == 15 {
                cpu.refill(bus, res);
            } else {
                cpu.r[rd] = res;
            }
        }
        1 => {
            let (r, c, v) = sub_with_carry(cpu.r[rd], b, 1);
            cpu.set_nz(r);
            cpu.set_c(c);
            cpu.set_v(v);
        }
        _ => {
            // BX: bit 0 of the target selects the new state.
            cpu.cpsr = (cpu.cpsr & !PSR_T) | ((b & 1) << 5);
            cpu.refill(bus, b);
        }
    }
}

/// Format 6: LDR Rd,[PC,#imm].
fn ldr_pc<B: CpuBus>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let addr = (cpu.r[15] & !2).wrapping_add((op & 0xFF) << 2);
    let v = bus.read32(addr, Access::NonSeq);
    cpu.internal(bus, 1);
    cpu.seq = false;
    cpu.r[lo(op, 8)] = v.rotate_right((addr & 3) * 8);
}

#[inline(always)]
fn load_word<B: CpuBus>(cpu: &mut Cpu, bus: &mut B, addr: u32, rd: usize) {
    let v = bus.read32(addr, Access::NonSeq);
    cpu.internal(bus, 1);
    cpu.seq = false;
    cpu.r[rd] = v.rotate_right((addr & 3) * 8);
}

#[inline(always)]
fn load_byte<B: CpuBus>(cpu: &mut Cpu, bus: &mut B, addr: u32, rd: usize) {
    let v = bus.read8(addr, Access::NonSeq);
    cpu.internal(bus, 1);
    cpu.seq = false;
    cpu.r[rd] = u32::from(v);
}

#[inline(always)]
fn load_half<B: CpuBus>(cpu: &mut Cpu, bus: &mut B, addr: u32, rd: usize) {
    let v = bus.read16(addr, Access::NonSeq);
    cpu.internal(bus, 1);
    cpu.seq = false;
    cpu.r[rd] = u32::from(v).rotate_right((addr & 1) * 8);
}

/// Formats 7 and 8: load/store with a register offset. `F` = opcode bits 11..9.
fn ls_reg<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let addr = cpu.r[lo(op, 3)].wrapping_add(cpu.r[lo(op, 6)]);
    let rd = lo(op, 0);
    match F {
        0 => {
            bus.write32(addr, cpu.r[rd], Access::NonSeq);
            cpu.seq = false;
        }
        1 => {
            bus.write16(addr, cpu.r[rd] as u16, Access::NonSeq);
            cpu.seq = false;
        }
        2 => {
            bus.write8(addr, cpu.r[rd] as u8, Access::NonSeq);
            cpu.seq = false;
        }
        3 => {
            let v = bus.read8(addr, Access::NonSeq);
            cpu.internal(bus, 1);
            cpu.seq = false;
            cpu.r[rd] = i32::from(v as i8) as u32;
        }
        4 => load_word(cpu, bus, addr, rd),
        5 => load_half(cpu, bus, addr, rd),
        6 => load_byte(cpu, bus, addr, rd),
        _ => {
            // LDSH: a misaligned address behaves like LDSB.
            let h = bus.read16(addr, Access::NonSeq);
            let v = if addr & 1 != 0 {
                i32::from((h >> 8) as i8) as u32
            } else {
                i32::from(h as i16) as u32
            };
            cpu.internal(bus, 1);
            cpu.seq = false;
            cpu.r[rd] = v;
        }
    }
}

/// Format 9: load/store with a 5-bit immediate offset. `F` = B (bit 1), L (bit 0).
fn ls_imm<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let imm = (op >> 6) & 0x1F;
    let rd = lo(op, 0);
    let base = cpu.r[lo(op, 3)];
    match F {
        0 => {
            bus.write32(base.wrapping_add(imm << 2), cpu.r[rd], Access::NonSeq);
            cpu.seq = false;
        }
        1 => load_word(cpu, bus, base.wrapping_add(imm << 2), rd),
        2 => {
            bus.write8(base.wrapping_add(imm), cpu.r[rd] as u8, Access::NonSeq);
            cpu.seq = false;
        }
        _ => load_byte(cpu, bus, base.wrapping_add(imm), rd),
    }
}

/// Format 10: LDRH/STRH with an immediate offset. `F` = L.
fn ls_half_imm<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let addr = cpu.r[lo(op, 3)].wrapping_add(((op >> 6) & 0x1F) << 1);
    let rd = lo(op, 0);
    if F != 0 {
        load_half(cpu, bus, addr, rd);
    } else {
        bus.write16(addr, cpu.r[rd] as u16, Access::NonSeq);
        cpu.seq = false;
    }
}

/// Format 11: SP-relative LDR/STR. `F` = L.
fn ls_sp<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let addr = cpu.r[13].wrapping_add((op & 0xFF) << 2);
    let rd = lo(op, 8);
    if F != 0 {
        load_word(cpu, bus, addr, rd);
    } else {
        bus.write32(addr, cpu.r[rd], Access::NonSeq);
        cpu.seq = false;
    }
}

/// Format 12: ADD Rd,PC/SP,#imm. `F` = SP.
fn add_pc_sp<B: CpuBus, const F: u32>(cpu: &mut Cpu, _bus: &mut B, op: u32) {
    let base = if F != 0 { cpu.r[13] } else { cpu.r[15] & !2 };
    cpu.r[lo(op, 8)] = base.wrapping_add((op & 0xFF) << 2);
}

/// Format 13: ADD SP,#+/-imm. `F` = negative.
fn add_sp<B: CpuBus, const F: u32>(cpu: &mut Cpu, _bus: &mut B, op: u32) {
    let off = (op & 0x7F) << 2;
    cpu.r[13] = if F != 0 {
        cpu.r[13].wrapping_sub(off)
    } else {
        cpu.r[13].wrapping_add(off)
    };
}

/// Format 14: PUSH/POP. `F` = L (bit 1), R (bit 0: LR for push, PC for pop).
/// Runs the ARM block-transfer handler (STMDB/LDMIA on SP with writeback).
fn push_pop<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let extra = if F & 1 != 0 {
        if F & 2 != 0 { 1 << 15 } else { 1 << 14 }
    } else {
        0
    };
    let list = (op & 0xFF) | extra | (13 << 16);
    if F & 2 != 0 {
        block_transfer::<B, 0b01011>(cpu, bus, list); // LDMIA sp!
    } else {
        block_transfer::<B, 0b10010>(cpu, bus, list); // STMDB sp!
    }
}

/// Format 15: LDMIA/STMIA with writeback. `F` = L.
fn ldm_stm<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let list = (op & 0xFF) | (((op >> 8) & 7) << 16);
    if F != 0 {
        block_transfer::<B, 0b01011>(cpu, bus, list);
    } else {
        block_transfer::<B, 0b01010>(cpu, bus, list);
    }
}

/// Format 16/17: conditional branch, and SWI (condition 0xF). `F` = condition.
fn bcc<B: CpuBus, const F: u32>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    if F == 0xF {
        cpu.software_interrupt(bus, op as u8);
    } else if F == 0xE || COND_PASS[F as usize] >> (cpu.cpsr >> 28) & 1 != 0 {
        let target = cpu.r[15].wrapping_add(((op as u8 as i8 as i32) << 1) as u32);
        cpu.refill(bus, target);
    }
}

/// Format 18: unconditional branch.
fn branch<B: CpuBus>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let off = (((op & 0x7FF) << 21) as i32 >> 20) as u32;
    let target = cpu.r[15].wrapping_add(off);
    cpu.refill(bus, target);
}

/// Format 19, first half: LR = PC + (offset high << 12).
fn bl_prefix<B: CpuBus>(cpu: &mut Cpu, _bus: &mut B, op: u32) {
    let off = (((op & 0x7FF) << 21) as i32 >> 9) as u32;
    cpu.r[14] = cpu.r[15].wrapping_add(off);
}

/// Format 19, second half: jump to LR + (offset low << 1), LR = return | 1.
fn bl_suffix<B: CpuBus>(cpu: &mut Cpu, bus: &mut B, op: u32) {
    let target = cpu.r[14].wrapping_add((op & 0x7FF) << 1);
    cpu.r[14] = cpu.r[15].wrapping_sub(2) | 1;
    cpu.refill(bus, target);
}
