//! Focused unit tests for the quirks the conformance suites do not name: mode
//! banking, LDM/STM corners, multiply timing, exceptions and the IRQ return
//! address. Code is a handful of hand-assembled words on a flat RAM bus.

use super::*;
use crate::bus::{Access, CpuBus, SwiResult};

struct Ram {
    mem: Vec<u8>,
    /// Internal cycles requested so far.
    idle: u32,
    /// Bus accesses so far (reads, writes and fetches).
    accesses: Vec<(char, u32, Access)>,
    irq: bool,
    swi: SwiResult,
}

impl Ram {
    fn new(code: &[u32]) -> Self {
        let mut mem = vec![0u8; 0x10_0000];
        for (i, w) in code.iter().enumerate() {
            mem[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        Self {
            mem,
            idle: 0,
            accesses: Vec::new(),
            irq: false,
            swi: SwiResult::Unhandled,
        }
    }

    fn word(&self, addr: u32) -> u32 {
        let a = addr as usize & 0xF_FFFC;
        u32::from_le_bytes(self.mem[a..a + 4].try_into().unwrap())
    }

    fn set_word(&mut self, addr: u32, v: u32) {
        let a = addr as usize & 0xF_FFFC;
        self.mem[a..a + 4].copy_from_slice(&v.to_le_bytes());
    }
}

impl CpuBus for Ram {
    fn read8(&mut self, addr: u32, access: Access) -> u8 {
        self.accesses.push(('r', addr, access));
        self.mem[addr as usize & 0xF_FFFF]
    }
    fn read16(&mut self, addr: u32, access: Access) -> u16 {
        self.accesses.push(('r', addr, access));
        let a = addr as usize & 0xF_FFFE;
        u16::from_le_bytes([self.mem[a], self.mem[a + 1]])
    }
    fn read32(&mut self, addr: u32, access: Access) -> u32 {
        self.accesses.push(('r', addr, access));
        self.word(addr)
    }
    fn write8(&mut self, addr: u32, v: u8, access: Access) {
        self.accesses.push(('w', addr, access));
        self.mem[addr as usize & 0xF_FFFF] = v;
    }
    fn write16(&mut self, addr: u32, v: u16, access: Access) {
        self.accesses.push(('w', addr, access));
        let a = addr as usize & 0xF_FFFE;
        self.mem[a..a + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn write32(&mut self, addr: u32, v: u32, access: Access) {
        self.accesses.push(('w', addr, access));
        self.set_word(addr, v);
    }
    fn idle(&mut self, cycles: u32) {
        self.idle += cycles;
    }
    fn irq_line(&self) -> bool {
        self.irq
    }
    fn swi(&mut self, _number: u8, _r: &mut [u32; 4]) -> SwiResult {
        self.swi
    }
}

/// Build a CPU in System mode at address 0 with `code` loaded there and apply
/// `poke` to its (user-view) state.
fn setup(code: &[u32], poke: impl FnOnce(&mut CpuState)) -> (Cpu, Ram) {
    let ram = Ram::new(code);
    let mut s = CpuState {
        cpsr: 0x1F,
        pipeline: [ram.word(0), ram.word(4)],
        ..CpuState::default()
    };
    s.r[15] = 8;
    poke(&mut s);
    let mut cpu = Cpu::reset();
    cpu.set_state(&s);
    (cpu, ram)
}

fn run(cpu: &mut Cpu, ram: &mut Ram, steps: usize) {
    for _ in 0..steps {
        cpu.step(ram);
    }
}

const MSR_CPSR_C: u32 = 0xE321_F000; // msr cpsr_c, #imm8
const MOV_R0_SP: u32 = 0xE1A0_000D;

#[test]
fn post_bios_has_documented_stacks_and_banks_on_mode_switch() {
    // The test RAM mirrors the ROM start (0x0800_0000) onto address 0.
    let mut ram = Ram::new(&[
        MSR_CPSR_C | 0x12,
        MOV_R0_SP,
        MSR_CPSR_C | 0x13,
        0xE1A0_100D, // mov r1, sp
        MSR_CPSR_C | 0x1F,
        0xE1A0_200D, // mov r2, sp
    ]);
    let mut cpu = Cpu::post_bios();
    run(&mut cpu, &mut ram, 6);
    let r = cpu.registers().r;
    assert_eq!(r[0], 0x0300_7FA0, "SP_irq");
    assert_eq!(r[1], 0x0300_7FE0, "SP_svc");
    assert_eq!(r[2], 0x0300_7F00, "SP_usr/sys");
}

#[test]
fn post_bios_and_reset_registers_before_the_first_step() {
    let r = Cpu::post_bios().registers();
    assert_eq!((r.cpsr, r.r[13], r.r[15]), (0x1F, 0x0300_7F00, 0x0800_0000));
    let r = Cpu::reset().registers();
    assert_eq!((r.cpsr, r.r[15]), (0xD3, 0));
}

#[test]
fn fiq_banks_r8_to_r14_and_restores_the_user_registers() {
    let (mut cpu, mut ram) = setup(
        &[
            0xE3A0_8088,       // mov r8, #0x88
            MSR_CPSR_C | 0x11, // fiq
            0xE1A0_0008,       // mov r0, r8   (FIQ's own r8: 0)
            0xE3A0_8011,       // mov r8, #0x11
            MSR_CPSR_C | 0x1F, // back to system
            0xE1A0_1008,       // mov r1, r8   (the user's r8 again)
        ],
        |_| {},
    );
    run(&mut cpu, &mut ram, 6);
    let r = cpu.registers().r;
    assert_eq!(r[0], 0);
    assert_eq!(r[1], 0x88);
    assert_eq!(cpu.state().r_fiq[0], 0x11, "FIQ r8 kept in its bank");
}

#[test]
fn msr_in_user_mode_only_changes_the_flags() {
    // msr cpsr_fc, #0x13: the mode bits are ignored in user mode.
    let (mut cpu, mut ram) = setup(&[0xE329_F013], |s| s.cpsr = 0x10);
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.registers().cpsr & 0x1F, 0x10);
}

#[test]
fn ldm_with_empty_list_loads_pc_and_moves_base_by_0x40() {
    // ldmia r0!, {}
    let (mut cpu, mut ram) = setup(&[0xE8B0_0000], |s| s.r[0] = 0x100);
    ram.set_word(0x100, 0x200);
    run(&mut cpu, &mut ram, 1);
    let r = cpu.registers().r;
    assert_eq!(r[0], 0x140);
    assert_eq!(r[15], 0x200);
}

#[test]
fn stm_with_empty_list_stores_pc_plus_12_and_moves_base() {
    // stmia r0!, {}
    let (mut cpu, mut ram) = setup(&[0xE8A0_0000], |s| s.r[0] = 0x100);
    run(&mut cpu, &mut ram, 1);
    assert_eq!(ram.word(0x100), 12);
    assert_eq!(cpu.registers().r[0], 0x140);
}

#[test]
fn stm_stores_old_base_if_lowest_else_new_base() {
    // stmia r0!, {r0,r1}; stmia r2!, {r1,r2}
    let (mut cpu, mut ram) = setup(&[0xE8A0_0003, 0xE8A2_0006], |s| {
        s.r[0] = 0x100;
        s.r[1] = 0xAAAA;
        s.r[2] = 0x200;
    });
    run(&mut cpu, &mut ram, 2);
    assert_eq!(ram.word(0x100), 0x100, "base first in the list: old value");
    assert_eq!(ram.word(0x200), 0xAAAA);
    assert_eq!(ram.word(0x204), 0x208, "base later in the list: new value");
}

#[test]
fn ldm_with_base_in_list_keeps_the_loaded_value() {
    // ldmia r0!, {r0,r1}
    let (mut cpu, mut ram) = setup(&[0xE8B0_0003], |s| s.r[0] = 0x100);
    ram.set_word(0x100, 0x1234);
    ram.set_word(0x104, 0x5678);
    run(&mut cpu, &mut ram, 1);
    let r = cpu.registers().r;
    assert_eq!((r[0], r[1]), (0x1234, 0x5678));
}

#[test]
fn ldm_access_pattern_is_n_then_s_then_one_internal_cycle() {
    // ldmia r0, {r1-r3}
    let (mut cpu, mut ram) = setup(&[0xE890_000E], |s| s.r[0] = 0x100);
    run(&mut cpu, &mut ram, 1);
    let data: Vec<_> = ram.accesses.iter().filter(|a| a.1 >= 0x100).collect();
    assert_eq!(data.len(), 3);
    assert_eq!(data[0].2, Access::NonSeq);
    assert_eq!(data[1].2, Access::Seq);
    assert_eq!(data[2].2, Access::Seq);
    assert_eq!(ram.idle, 1);
}

#[test]
fn ldm_with_s_bit_and_pc_restores_cpsr_from_spsr() {
    // ldmia sp!, {pc}^ from IRQ mode with SPSR_irq = system mode, flags NZ... set.
    let (mut cpu, mut ram) = setup(&[0xE8FD_8000], |s| {
        s.cpsr = 0x12;
        s.r_irq = [0x100, 0];
        s.spsr[3] = 0x6000_001F;
    });
    ram.set_word(0x100, 0x300);
    run(&mut cpu, &mut ram, 1);
    let regs = cpu.registers();
    assert_eq!(regs.cpsr, 0x6000_001F);
    assert_eq!(regs.r[15], 0x300);
}

#[test]
fn ldm_with_s_bit_transfers_the_user_bank() {
    // ldmia r0, {r13,r14}^ from IRQ mode writes the user r13/r14.
    let (mut cpu, mut ram) = setup(&[0xE8D0_6000], |s| {
        s.cpsr = 0x12;
        s.r[0] = 0x100;
        s.r_irq = [0x1111, 0x2222];
    });
    ram.set_word(0x100, 0xAAAA);
    ram.set_word(0x104, 0xBBBB);
    run(&mut cpu, &mut ram, 1);
    let st = cpu.state();
    assert_eq!((st.r[13], st.r[14]), (0xAAAA, 0xBBBB), "user bank");
    assert_eq!(st.r_irq, [0x1111, 0x2222], "IRQ bank untouched");
}

#[test]
fn ldr_from_a_misaligned_address_rotates() {
    // ldr r0, [r1]; ldrb r2, [r1]
    let (mut cpu, mut ram) = setup(&[0xE591_0000, 0xE5D1_2000], |s| s.r[1] = 0x101);
    ram.set_word(0x100, 0x4433_2211);
    run(&mut cpu, &mut ram, 2);
    let r = cpu.registers().r;
    assert_eq!(r[0], 0x1144_3322);
    assert_eq!(r[2], 0x22);
}

#[test]
fn ldrsh_on_an_odd_address_sign_extends_the_byte() {
    // ldrsh r0, [r1]
    let (mut cpu, mut ram) = setup(&[0xE1D1_00F0], |s| s.r[1] = 0x101);
    ram.set_word(0x100, 0x0000_80FF);
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.registers().r[0], 0xFFFF_FF80);
}

#[test]
fn multiply_takes_one_to_four_internal_cycles() {
    for (rs, want) in [
        (0xFFu32, 1),
        (0xFFFF_FF00, 1),
        (0x1234, 2),
        (0x12_3456, 3),
        (0x1234_5678, 4),
    ] {
        // mul r0, r1, r2
        let (mut cpu, mut ram) = setup(&[0xE000_0291], |s| {
            s.r[1] = 3;
            s.r[2] = rs;
        });
        run(&mut cpu, &mut ram, 1);
        assert_eq!(ram.idle, want, "multiplier {rs:#X}");
        assert_eq!(cpu.registers().r[0], 3u32.wrapping_mul(rs));
    }
}

#[test]
fn mla_and_long_multiplies_add_internal_cycles() {
    // mla r0, r1, r2, r3; umull r4, r5, r1, r2; smlal r6, r7, r1, r2
    let (mut cpu, mut ram) = setup(&[0xE020_3291, 0xE085_4291, 0xE0E7_6291], |s| {
        s.r[1] = 2;
        s.r[2] = 5;
        s.r[3] = 7;
    });
    run(&mut cpu, &mut ram, 1);
    assert_eq!((cpu.registers().r[0], ram.idle), (17, 2), "MLA: m + 1");
    run(&mut cpu, &mut ram, 1);
    assert_eq!(ram.idle, 2 + 2, "UMULL: m + 1");
    run(&mut cpu, &mut ram, 1);
    assert_eq!(ram.idle, 2 + 2 + 3, "SMLAL: m + 2");
}

#[test]
fn flags_after_adds_and_sbc() {
    // adds r0, r1, r2; sbcs r3, r4, r5
    let (mut cpu, mut ram) = setup(&[0xE091_0002, 0xE0D4_3005], |s| {
        s.r[1] = 0x7FFF_FFFF;
        s.r[2] = 1;
        s.r[4] = 5;
        s.r[5] = 5;
    });
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.registers().cpsr >> 28, 0b1001, "N and V set");
    run(&mut cpu, &mut ram, 1);
    // C is clear, so 5 - 5 - 1 = -1 with a borrow.
    let r = cpu.registers();
    assert_eq!(r.r[3], 0xFFFF_FFFF);
    assert_eq!(r.cpsr >> 28, 0b1000);
}

#[test]
fn register_shift_by_32_gives_zero_and_carry_from_bit_0() {
    // movs r0, r1, lsl r2 with r2 = 32
    let (mut cpu, mut ram) = setup(&[0xE1B0_0211], |s| {
        s.r[1] = 0xFFFF_FFFF;
        s.r[2] = 32;
    });
    run(&mut cpu, &mut ram, 1);
    let r = cpu.registers();
    assert_eq!(r.r[0], 0);
    assert_eq!(r.cpsr >> 28, 0b0110, "Z and C");
    assert_eq!(ram.idle, 1, "a register shift costs an internal cycle");
}

#[test]
fn pc_reads_plus_8_and_plus_12_for_register_shifts() {
    // mov r0, pc; mov r1, pc, lsl r2 (r2 = 0)
    let (mut cpu, mut ram) = setup(&[0xE1A0_000F, 0xE1A0_121F], |_| {});
    run(&mut cpu, &mut ram, 2);
    let r = cpu.registers().r;
    assert_eq!(r[0], 8);
    assert_eq!(r[1], 4 + 12);
}

#[test]
fn branch_refills_with_one_n_and_one_s_fetch() {
    // b .+8: the normal prefetch (N: setup left it non-sequential), then the
    // refill's N and S.
    let (mut cpu, mut ram) = setup(&[0xEA00_0000], |_| {});
    run(&mut cpu, &mut ram, 1);
    let kinds: Vec<_> = ram.accesses.iter().map(|a| a.2).collect();
    assert_eq!(kinds, [Access::NonSeq, Access::NonSeq, Access::Seq]);
    assert_eq!(cpu.registers().r[15], 8);
}

#[test]
fn registers_reports_the_next_instruction_not_the_pipelined_pc() {
    let (mut cpu, mut ram) = setup(&[0xE1A0_0000, 0xE1A0_0000], |_| {});
    assert_eq!(cpu.registers().r[15], 0);
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.registers().r[15], 4);
}

#[test]
fn unhandled_swi_enters_svc_with_lr_after_the_swi() {
    // swi 0x12 at address 0, from system mode.
    let (mut cpu, mut ram) = setup(&[0xEF00_0012], |_| {});
    run(&mut cpu, &mut ram, 1);
    let r = cpu.registers();
    assert_eq!(r.cpsr & 0x1F, 0x13);
    assert_ne!(r.cpsr & 0x80, 0, "IRQs masked");
    assert_eq!(r.r[14], 4);
    assert_eq!(r.r[15], 8, "SWI vector");
    assert_eq!(cpu.state().spsr[1], 0x1F, "SPSR_svc holds the old CPSR");
}

#[test]
fn hle_swi_done_and_wait_then_retry() {
    let (mut cpu, mut ram) = setup(&[0xEF00_0000, 0xE1A0_0000], |_| {});
    ram.swi = SwiResult::Done;
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.registers().r[15], 4, "Done continues after the SWI");

    let (mut cpu, mut ram) = setup(&[0xEF00_0000, 0xE1A0_0000], |_| {});
    ram.swi = SwiResult::WaitThenRetry;
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.registers().r[15], 0, "WaitThenRetry stays on the SWI");
    ram.swi = SwiResult::Done;
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.registers().r[15], 4, "re-executes and now completes");
}

#[test]
fn undefined_instruction_takes_the_und_exception() {
    let (mut cpu, mut ram) = setup(&[0xE7F0_00F0], |_| {});
    run(&mut cpu, &mut ram, 1);
    let r = cpu.registers();
    assert_eq!(r.cpsr & 0x1F, 0x1B);
    assert_eq!((r.r[14], r.r[15]), (4, 4));
}

#[test]
fn irq_return_address_is_next_instruction_plus_4_in_arm_and_thumb() {
    // ARM: IRQ pending before the instruction at 0x0.
    let (mut cpu, mut ram) = setup(&[0xE1A0_0000; 4], |_| {});
    ram.irq = true;
    run(&mut cpu, &mut ram, 1);
    let r = cpu.registers();
    assert_eq!(r.cpsr & 0x1F, 0x12);
    assert_eq!(r.r[14] - 4, 0, "`subs pc, lr, #4` resumes the interrupted instruction");
    assert_eq!(r.r[15], 0x18);
    assert_eq!(cpu.state().spsr[3], 0x1F);

    // Thumb: instruction at 0x2, so the return target is 0x2.
    let (mut cpu, mut ram) = setup(&[0; 4], |s| {
        s.cpsr = 0x3F;
        s.r[15] = 2 + 4;
        s.pipeline = [0, 0];
    });
    ram.irq = true;
    run(&mut cpu, &mut ram, 1);
    let r = cpu.registers();
    assert_eq!(r.cpsr & 0x3F, 0x12, "ARM state in IRQ mode");
    assert_eq!(r.r[14] - 4, 2);
}

#[test]
fn irq_is_ignored_while_cpsr_i_is_set() {
    let (mut cpu, mut ram) = setup(&[0xE1A0_0000; 2], |s| s.cpsr = 0x9F);
    ram.irq = true;
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.registers().cpsr & 0x1F, 0x1F);
    assert_eq!(cpu.registers().r[15], 4);
}

#[test]
fn bx_switches_to_thumb_and_back() {
    // bx r0 with r0 = 0x101 (Thumb at 0x100), where the Thumb code is `bx r1`
    // (0x4708) with r1 = 0x200 (ARM).
    let (mut cpu, mut ram) = setup(&[0xE12F_FF10], |s| {
        s.r[0] = 0x101;
        s.r[1] = 0x200;
    });
    ram.mem[0x100] = 0x08;
    ram.mem[0x101] = 0x47;
    run(&mut cpu, &mut ram, 1);
    assert_ne!(cpu.registers().cpsr & 0x20, 0);
    assert_eq!(cpu.registers().r[15], 0x100);
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.registers().cpsr & 0x20, 0);
    assert_eq!(cpu.registers().r[15], 0x200);
}

#[test]
fn thumb_bl_pair_links_and_jumps() {
    // At 0x100: bl +0x10 (prefix F000, suffix F808).
    let (mut cpu, mut ram) = setup(&[0; 1], |s| {
        s.cpsr = 0x3F;
        s.r[15] = 0x100 + 4;
        s.pipeline = [0xF000, 0xF808];
    });
    run(&mut cpu, &mut ram, 2);
    let r = cpu.registers();
    assert_eq!(r.r[14], 0x104 | 1);
    assert_eq!(r.r[15], 0x114);
}

#[test]
fn thumb_push_pop_and_pop_pc() {
    // push {r0,lr}; pop {r1,pc}
    let (mut cpu, mut ram) = setup(&[0], |s| {
        s.cpsr = 0x3F;
        s.r[13] = 0x1000;
        s.r[0] = 0x55;
        s.r[14] = 0x300;
        s.r[15] = 4;
        s.pipeline = [0xB501, 0xBD02];
    });
    run(&mut cpu, &mut ram, 2);
    let r = cpu.registers();
    assert_eq!(r.r[13], 0x1000);
    assert_eq!(r.r[1], 0x55);
    assert_eq!(r.r[15], 0x300);
}

#[test]
fn thumb_hi_register_add_to_pc_branches() {
    // add pc, r0 at 0: pc reads 4, so the target is 4 + r0.
    let (mut cpu, mut ram) = setup(&[0], |s| {
        s.cpsr = 0x3F;
        s.r[0] = 0x20;
        s.r[15] = 4;
        s.pipeline = [0x4487, 0];
    });
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.registers().r[15], 0x24);
}
