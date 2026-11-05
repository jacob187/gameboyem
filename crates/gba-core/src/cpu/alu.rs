//! Arithmetic, the barrel shifter and the multiplier cycle rule (GBATEK "ARM
//! Opcodes: Data Processing", "ARM CPU Instruction Cycle Times").

/// Result of an add-style operation: value, carry, signed overflow.
#[inline(always)]
pub fn add_with_carry(a: u32, b: u32, carry_in: u32) -> (u32, bool, bool) {
    let wide = u64::from(a) + u64::from(b) + u64::from(carry_in);
    let res = wide as u32;
    let carry = wide > 0xFFFF_FFFF;
    let overflow = (!(a ^ b) & (a ^ res)) >> 31 != 0;
    (res, carry, overflow)
}

/// `a - b - !carry_in` (SUB uses carry_in = 1, SBC the C flag).
#[inline(always)]
pub fn sub_with_carry(a: u32, b: u32, carry_in: u32) -> (u32, bool, bool) {
    let (res, carry, _) = add_with_carry(a, !b, carry_in);
    let overflow = ((a ^ b) & (a ^ res)) >> 31 != 0;
    (res, carry, overflow)
}

/// Barrel shifter kinds (opcode bits 6:5); anything else is ROR.
const LSL: u32 = 0;
const LSR: u32 = 1;
const ASR: u32 = 2;

/// Shift by an immediate amount (opcode bits 11:7). `kind` is opcode bits 6:5:
/// 0 LSL, 1 LSR, 2 ASR, 3 ROR. The encodings LSR #0 and
/// ASR #0 mean #32 and ROR #0 means RRX. Returns (value, carry out); `c` is the
/// current C flag.
#[inline(always)]
pub fn shift_imm(kind: u32, v: u32, amount: u32, c: bool) -> (u32, bool) {
    match kind {
        LSL => {
            if amount == 0 {
                (v, c)
            } else {
                (v << amount, (v >> (32 - amount)) & 1 != 0)
            }
        }
        LSR => {
            if amount == 0 {
                (0, v >> 31 != 0)
            } else {
                (v >> amount, (v >> (amount - 1)) & 1 != 0)
            }
        }
        ASR => {
            if amount == 0 {
                (((v as i32) >> 31) as u32, v >> 31 != 0)
            } else {
                (((v as i32) >> amount) as u32, (v >> (amount - 1)) & 1 != 0)
            }
        }
        _ => {
            if amount == 0 {
                ((v >> 1) | (u32::from(c) << 31), v & 1 != 0)
            } else {
                (v.rotate_right(amount), (v >> (amount - 1)) & 1 != 0)
            }
        }
    }
}

/// Shift by a register (only the low 8 bits of Rs count). Amount 0 leaves the
/// value and C alone; 32 and above follow the ARM rules (LSL/LSR give 0, ASR
/// replicates the sign, ROR wraps modulo 32).
#[inline(always)]
pub fn shift_reg(kind: u32, v: u32, amount: u32, c: bool) -> (u32, bool) {
    let n = amount & 0xFF;
    if n == 0 {
        return (v, c);
    }
    match kind {
        LSL => match n {
            1..=31 => (v << n, (v >> (32 - n)) & 1 != 0),
            32 => (0, v & 1 != 0),
            _ => (0, false),
        },
        LSR => match n {
            1..=31 => (v >> n, (v >> (n - 1)) & 1 != 0),
            32 => (0, v >> 31 != 0),
            _ => (0, false),
        },
        ASR => {
            if n < 32 {
                (((v as i32) >> n) as u32, (v >> (n - 1)) & 1 != 0)
            } else {
                (((v as i32) >> 31) as u32, v >> 31 != 0)
            }
        }
        _ => {
            let r = n & 31;
            if r == 0 {
                (v, v >> 31 != 0)
            } else {
                (v.rotate_right(r), (v >> (r - 1)) & 1 != 0)
            }
        }
    }
}

/// Internal cycles of a multiply: 1..=4 depending on how many high bytes of
/// the multiplier `rs` are all zeros (or, for signed multiplies, all ones).
#[inline(always)]
pub fn mul_cycles(rs: u32, signed: bool) -> u32 {
    let ones = if signed { !rs } else { u32::MAX };
    if rs >> 8 == 0 || ones >> 8 == 0 {
        1
    } else if rs >> 16 == 0 || ones >> 16 == 0 {
        2
    } else if rs >> 24 == 0 || ones >> 24 == 0 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROR: u32 = 3;

    #[test]
    fn add_sub_flags() {
        assert_eq!(add_with_carry(0xFFFF_FFFF, 1, 0), (0, true, false));
        assert_eq!(
            add_with_carry(0x7FFF_FFFF, 1, 0),
            (0x8000_0000, false, true)
        );
        assert_eq!(sub_with_carry(5, 5, 1), (0, true, false));
        assert_eq!(sub_with_carry(0, 1, 1), (0xFFFF_FFFF, false, false));
        assert_eq!(sub_with_carry(0x8000_0000, 1, 1), (0x7FFF_FFFF, true, true));
    }

    #[test]
    fn shifter_by_register_32_and_up() {
        assert_eq!(shift_reg(LSL, 0xFFFF_FFFF, 32, false), (0, true));
        assert_eq!(shift_reg(LSL, 0xFFFF_FFFF, 33, true), (0, false));
        assert_eq!(shift_reg(LSR, 0x8000_0000, 32, false), (0, true));
        assert_eq!(shift_reg(LSR, 0x8000_0000, 40, true), (0, false));
        assert_eq!(shift_reg(ASR, 0x8000_0000, 32, false), (0xFFFF_FFFF, true));
        assert_eq!(shift_reg(ASR, 0x7FFF_FFFF, 99, true), (0, false));
        assert_eq!(shift_reg(ROR, 0x8000_0001, 32, false), (0x8000_0001, true));
        assert_eq!(shift_reg(ROR, 0x8000_0001, 33, false), (0xC000_0000, true));
        // Amount 0 (low byte) keeps the carry; the upper bits of Rs are ignored.
        assert_eq!(shift_reg(LSL, 7, 0x100, true), (7, true));
    }

    #[test]
    fn immediate_shift_special_encodings() {
        assert_eq!(shift_imm(LSR, 0x8000_0000, 0, false), (0, true));
        assert_eq!(shift_imm(ASR, 0x8000_0000, 0, false), (0xFFFF_FFFF, true));
        assert_eq!(shift_imm(ROR, 3, 0, true), (0x8000_0001, true));
        assert_eq!(shift_imm(LSL, 3, 0, true), (3, true));
    }

    #[test]
    fn multiply_early_termination() {
        assert_eq!(mul_cycles(0xFF, false), 1);
        assert_eq!(mul_cycles(0xFFFF_FF00, true), 1);
        assert_eq!(mul_cycles(0xFFFF_FF00, false), 4);
        assert_eq!(mul_cycles(0x1_0000, false), 3);
        assert_eq!(mul_cycles(0x0100_0000, true), 4);
    }
}
