//! The built-in BIOS image: our own ARM code for the HLE BIOS.
//!
//! It is assembled at compile time by a tiny `const fn` assembler (every
//! immediate that cannot be encoded is a compile error). Nothing here is
//! Nintendo's code: the instruction sequences come from GBATEK's descriptions
//! ("BIOS Reset Functions" for SoftReset, "ARM CPU Exceptions" for the vectors,
//! "Interrupt Control" for the IRQ handler) and the word values at the four
//! latch addresses are the ones GBATEK's "Reading from BIOS Memory" and
//! jsmolka's `bios.asm` expect.
//!
//! # Layout
//!
//! ```text
//! 0x00  b softreset            reset vector
//! 0x04  movs pc, lr            undefined instruction: skip it
//! 0x08  b swi_entry            SWI vector
//! 0x0C  b .   0x10 b .  0x14 b .   aborts (not raised by this CPU)
//! 0x18  b irq_entry            IRQ vector
//! 0x1C  subs pc, lr, #4        FIQ (unused on the GBA)
//! 0x50  softreset ... 0xDC bx lr
//! 0xE4  msr cpsr_fc, r0        latch word, never executed
//! 0x128 irq_entry: stmfd, mov r0,#0x04000000, add lr,pc,#0,
//!       0x134 ldr pc,[r0,#-4]   (the user handler at 0x0300_7FFC)
//!       0x138 ldmfd
//!       0x13C subs pc, lr, #4
//! 0x140 swi_entry: stmfd, 0x144 ldrb r12,[lr,#-2] (comment byte), compare
//! 0x184 ldmfd, 0x188 movs pc, lr (SWI return)
//! 0x190 mov r2, #4             latch word, never executed
//! ```
//!
//! # Read-protection latch values
//!
//! The latch is the last word fetched from the BIOS. A fetch of address `X`
//! happens while the instruction at `X - 8` executes, so these words give:
//!
//! | moment                      | executing | latch word at | value        |
//! |-----------------------------|-----------|---------------|--------------|
//! | after startup / SoftReset   | 0x0DC     | 0x0E4         | 0xE129_F000  |
//! | during the user IRQ handler | 0x134     | 0x13C         | 0xE25E_F004  |
//! | after the IRQ returns       | 0x13C     | 0x144         | 0xE55E_C002  |
//! | after an SWI returns        | 0x188     | 0x190         | 0xE3A0_2004  |
//!
//! The IRQ rows fall out of the real code path. The SoftReset and SWI rows are
//! produced by the image too when the SWI goes through the vector; an HLE call
//! that finishes in Rust sets the latch to the SWI value itself (`swi`).

/// The image is `WORDS` words long (everything above is zero).
pub const WORDS: usize = 0x198 / 4;

/// Latch value after startup and after SoftReset.
pub const LATCH_STARTUP: u32 = 0xE129_F000;
/// Latch value while the user IRQ handler runs.
pub const LATCH_IN_IRQ: u32 = 0xE25E_F004;
/// Latch value after the IRQ handler returned through the BIOS.
pub const LATCH_AFTER_IRQ: u32 = 0xE55E_C002;
/// Latch value after an SWI returned.
pub const LATCH_AFTER_SWI: u32 = 0xE3A0_2004;

// ---- the const assembler ----------------------------------------------------

const EQ: u32 = 0x0;
const NE: u32 = 0x1;
const AL: u32 = 0xE;

const SUB: u32 = 2;
const ADD: u32 = 4;
const CMP: u32 = 10;
const MOV: u32 = 13;

const SP: u32 = 13;
const LR: u32 = 14;
const PC: u32 = 15;

/// Encode `v` as an ARM rotated 8-bit immediate (rot in bits 8..12), or fail the
/// build.
const fn imm(v: u32) -> u32 {
    let mut rot = 0;
    while rot < 16 {
        let r = v.rotate_left(rot * 2);
        if r <= 0xFF {
            return rot << 8 | r;
        }
        rot += 1;
    }
    panic!("immediate not encodable");
}

/// Data processing with an immediate operand.
const fn dp_i(cond: u32, op: u32, s: u32, rn: u32, rd: u32, v: u32) -> u32 {
    cond << 28 | 1 << 25 | op << 21 | s << 20 | rn << 16 | rd << 12 | imm(v)
}

/// LDR/STR/LDRB/STRB with an immediate offset. `pre` = pre-indexed (no
/// writeback), otherwise post-indexed.
const fn ldst(load: bool, byte: bool, pre: bool, rn: u32, rd: u32, off: i32) -> u32 {
    let up = off >= 0;
    let mag = if up { off as u32 } else { (-off) as u32 };
    assert!(mag < 0x1000);
    0xE000_0000
        | 1 << 26
        | (pre as u32) << 24
        | (up as u32) << 23
        | (byte as u32) << 22
        | (load as u32) << 20
        | rn << 16
        | rd << 12
        | mag
}

/// `b{cond}` from the instruction at `at` to `to`.
const fn branch(cond: u32, at: u32, to: u32) -> u32 {
    let off = (to as i32 - (at as i32 + 8)) >> 2;
    cond << 28 | 0x0A00_0000 | (off as u32 & 0x00FF_FFFF)
}

const fn cmp_rr(rn: u32, rm: u32) -> u32 {
    0xE150_0000 | rn << 16 | rm
}

const fn bx(rm: u32) -> u32 {
    0xE12F_FF10 | rm
}

/// `msr cpsr_c, #v`
const fn msr_cpsr_c(v: u32) -> u32 {
    0xE321_F000 | imm(v)
}

/// `msr spsr_fc, rm`
const fn msr_spsr_fc(rm: u32) -> u32 {
    0xE169_F000 | rm
}

/// `msr cpsr_fc, rm`
const fn msr_cpsr_fc(rm: u32) -> u32 {
    0xE129_F000 | rm
}

const STMFD_R11_R12_LR: u32 = 0xE92D_5800;
const LDMFD_R11_R12_LR: u32 = 0xE8BD_5800;
/// `stmfd sp!, {r0-r3, r12, lr}` / `ldmfd sp!, {r0-r3, r12, lr}` (GBATEK
/// "BIOS RAM Usage" / "Interrupt Control": the BIOS IRQ handler).
const STMFD_IRQ: u32 = 0xE92D_500F;
const LDMFD_IRQ: u32 = 0xE8BD_500F;
const NOP: u32 = 0xE1A0_0000; // mov r0, r0

struct Asm {
    w: [u32; WORDS],
    at: u32,
}

impl Asm {
    const fn new() -> Self {
        Self {
            w: [0; WORDS],
            at: 0,
        }
    }
    const fn org(mut self, at: u32) -> Self {
        self.at = at;
        self
    }
    const fn emit(mut self, v: u32) -> Self {
        self.w[(self.at / 4) as usize] = v;
        self.at += 4;
        self
    }
    /// Emit a branch to `to` with condition `cond`.
    const fn b(self, cond: u32, to: u32) -> Self {
        let at = self.at;
        self.emit(branch(cond, at, to))
    }
    const fn here(&self) -> u32 {
        self.at
    }
}

const SOFTRESET: u32 = 0x50;
const IRQ_ENTRY: u32 = 0x128;
const SWI_ENTRY: u32 = 0x140;
const SWI_RETURN: u32 = 0x184;

const fn build() -> [u32; WORDS] {
    let a = Asm::new()
        // ---- exception vectors (GBATEK "ARM CPU Exceptions")
        .b(AL, SOFTRESET) // 0x00 reset
        .emit(0xE1B0_F00E) // 0x04 undefined: movs pc, lr (skip the instruction)
        .b(AL, SWI_ENTRY) // 0x08 SWI
        .b(AL, 0x0C) // 0x0C prefetch abort: not raised by this CPU
        .b(AL, 0x10) // 0x10 data abort: likewise
        .b(AL, 0x14) // 0x14 reserved
        .b(AL, IRQ_ENTRY) // 0x18 IRQ
        .emit(dp_i(AL, SUB, 1, LR, PC, 4)); // 0x1C FIQ: subs pc, lr, #4

    // ---- SoftReset (GBATEK "BIOS Reset Functions"), SVC mode on entry. Ends in
    // `bx lr` at 0xDC so the latch afterwards is the word at 0xE4.
    let a = a
        .org(SOFTRESET)
        .emit(dp_i(AL, MOV, 0, 0, 0, 0x0300_0000)) //          mov r0, #0x03000000
        .emit(dp_i(AL, ADD, 0, 0, 0, 0x7F00)) //                add r0, r0, #0x7F00 (sp_sys)
        .emit(ldst(true, true, true, 0, 2, 0xFA)); //           ldrb r2, [r0, #0xFA] (return flag)
    let a = a
        .emit(dp_i(AL, MOV, 0, 0, 1, 0)) //                     mov r1, #0
        .emit(dp_i(AL, SUB, 0, 0, 3, 0x100)) //                 sub r3, r0, #0x100 (0x03007E00)
        .emit(dp_i(AL, ADD, 0, 0, 4, 0x100)); //                add r4, r0, #0x100 (0x03008000)
    let clr = a.here();
    let a = a
        .emit(ldst(false, false, false, 3, 1, 4)) //            clr: str r1, [r3], #4
        .emit(cmp_rr(3, 4)) //                                  cmp r3, r4
        .b(NE, clr) //                                          bne clr
        .emit(msr_cpsr_c(0xD2)) //                              IRQ mode, I and F set
        .emit(dp_i(AL, ADD, 0, 0, SP, 0xA0)) //                 sp_irq = 0x03007FA0
        .emit(dp_i(AL, MOV, 0, 0, LR, 0)) //                    mov lr, #0
        .emit(msr_spsr_fc(1)) //                                spsr_irq = 0
        .emit(msr_cpsr_c(0xD3)) //                              SVC mode
        .emit(dp_i(AL, ADD, 0, 0, SP, 0xE0)) //                 sp_svc = 0x03007FE0
        .emit(dp_i(AL, MOV, 0, 0, LR, 0)) //                    mov lr, #0
        .emit(msr_spsr_fc(1)) //                                spsr_svc = 0
        .emit(msr_cpsr_c(0x1F)) //                              System mode, IRQ and FIQ enabled
        .emit(0xE1A0_D000) //                                   mov sp, r0 (sp_sys = 0x03007F00)
        .emit(dp_i(AL, CMP, 1, 2, 0, 0)) //                     cmp r2, #0
        .emit(dp_i(EQ, MOV, 0, 0, LR, 0x0800_0000)) //          moveq lr, #0x08000000 (ROM)
        .emit(dp_i(NE, MOV, 0, 0, LR, 0x0200_0000)); //         movne lr, #0x02000000 (EWRAM)
    // mov r0..r12, #0 (r1 last is fine: every move is an immediate)
    let mut a = a;
    let mut r = 0;
    while r <= 12 {
        a = a.emit(dp_i(AL, MOV, 0, 0, r, 0));
        r += 1;
    }
    let a = a.emit(bx(LR)); // must land on 0xDC
    assert!(a.here() == 0xE0);
    let a = a
        .emit(NOP) // 0xE0
        .emit(msr_cpsr_fc(0)); // 0xE4: LATCH_STARTUP, never executed

    // ---- IRQ entry (GBATEK "BIOS RAM Usage": handler pointer at 0x03007FFC)
    let a = a
        .org(IRQ_ENTRY)
        .emit(STMFD_IRQ) //                                     0x128 stmfd sp!, {r0-r3,r12,lr}
        .emit(dp_i(AL, MOV, 0, 0, 0, 0x0400_0000)) //           0x12C mov r0, #0x04000000
        .emit(dp_i(AL, ADD, 0, PC, LR, 0)) //                   0x130 add lr, pc, #0
        .emit(ldst(true, false, true, 0, PC, -4)) //            0x134 ldr pc, [r0, #-4]
        .emit(LDMFD_IRQ) //                                     0x138 ldmfd sp!, {r0-r3,r12,lr}
        .emit(dp_i(AL, SUB, 1, LR, PC, 4)); //                  0x13C subs pc, lr, #4
    assert!(a.here() == 0x140);

    // ---- SWI entry, reached only for SWIs the HLE leaves to the image: 0x00
    // (SoftReset) and 0x26 (HardReset) reset, anything else returns.
    let a = a
        .emit(STMFD_R11_R12_LR) //                              0x140 stmfd sp!, {r11,r12,lr}
        .emit(ldst(true, true, true, LR, 12, -2)) //            0x144 ldrb r12, [lr, #-2]
        .emit(dp_i(AL, CMP, 1, 12, 0, 0)) //                    0x148 cmp r12, #0
        .emit(dp_i(NE, CMP, 1, 12, 0, 0x26)) //                 0x14C cmpne r12, #0x26
        .b(EQ, SOFTRESET) //                                    0x150 beq softreset
        .b(AL, SWI_RETURN); //                                  0x154 b swi_return
    let a = a
        .org(SWI_RETURN)
        .emit(LDMFD_R11_R12_LR) //                              0x184 ldmfd sp!, {r11,r12,lr}
        .emit(0xE1B0_F00E) //                                   0x188 movs pc, lr
        .emit(NOP) //                                           0x18C
        .emit(dp_i(AL, MOV, 0, 0, 2, 4)); //                    0x190 mov r2, #4: LATCH_AFTER_SWI
    a.w
}

/// The image, as little-endian words from address 0.
pub const IMAGE: [u32; WORDS] = build();

/// Byte `addr` of the image (zero past the end of the code).
pub fn byte(addr: u32) -> u8 {
    let w = IMAGE.get((addr / 4) as usize).copied().unwrap_or(0);
    (w >> ((addr & 3) * 8)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(addr: u32) -> u32 {
        IMAGE[(addr / 4) as usize]
    }

    #[test]
    fn latch_words_sit_where_the_fetch_pipeline_reads_them() {
        assert_eq!(word(0xDC), bx(LR));
        assert_eq!(word(0xE4), LATCH_STARTUP);
        assert_eq!(word(0x134), 0xE510_F004); // ldr pc, [r0, #-4]
        assert_eq!(word(0x13C), LATCH_IN_IRQ);
        assert_eq!(word(0x144), LATCH_AFTER_IRQ);
        assert_eq!(word(0x188), 0xE1B0_F00E); // movs pc, lr
        assert_eq!(word(0x190), LATCH_AFTER_SWI);
    }

    #[test]
    fn encoders_match_known_opcodes() {
        assert_eq!(dp_i(AL, MOV, 0, 0, 0, 0x0400_0000), 0xE3A0_0301);
        assert_eq!(dp_i(AL, ADD, 0, PC, LR, 0), 0xE28F_E000);
        assert_eq!(ldst(true, true, true, LR, 12, -2), LATCH_AFTER_IRQ);
        assert_eq!(dp_i(AL, MOV, 0, 0, 2, 4), LATCH_AFTER_SWI);
        assert_eq!(msr_cpsr_fc(0), LATCH_STARTUP);
        assert_eq!(branch(AL, 0x10, 0x10), 0xEAFF_FFFE);
    }
}
