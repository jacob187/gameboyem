//! CPU unit tests on a flat 64 KiB bus that counts and logs M-cycles.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ev {
    Read(u16),
    Write(u16, u8),
    Tick,
}

/// Flat memory; 0xFFFF is IE and 0xFF0F is IF so interrupt behaviour (including
/// the IE-written-during-push quirk) is observable.
struct TestBus {
    mem: Vec<u8>,
    ie: u8,
    iflag: u8,
    log: Vec<Ev>,
    stopped: bool,
    joypad_low: bool,
}

impl TestBus {
    fn new() -> Self {
        Self {
            mem: vec![0; 0x1_0000],
            ie: 0,
            iflag: 0,
            log: Vec::new(),
            stopped: false,
            joypad_low: false,
        }
    }

    fn cycles(&self) -> usize {
        self.log.len()
    }
}

impl CpuBus for TestBus {
    fn read(&mut self, addr: u16) -> u8 {
        self.log.push(Ev::Read(addr));
        match addr {
            0xFFFF => self.ie,
            0xFF0F => self.iflag,
            _ => self.mem[addr as usize],
        }
    }

    fn write(&mut self, addr: u16, value: u8) {
        self.log.push(Ev::Write(addr, value));
        match addr {
            0xFFFF => self.ie = value,
            0xFF0F => self.iflag = value & 0x1F,
            _ => self.mem[addr as usize] = value,
        }
    }

    fn tick(&mut self) {
        self.log.push(Ev::Tick);
    }

    fn pending_interrupts(&self) -> u8 {
        self.ie & self.iflag & 0x1F
    }

    fn acknowledge_interrupt(&mut self, mask: u8) {
        self.iflag &= !mask;
    }

    fn stop(&mut self) -> bool {
        self.stopped = true;
        false
    }

    fn joypad_line_low(&self) -> bool {
        self.joypad_low
    }
}

const START: u16 = 0x0200;

/// CPU at `START` with `code` loaded there; SP=0xD000, HL=0xC000.
fn setup(code: &[u8]) -> (Cpu, TestBus) {
    let mut bus = TestBus::new();
    bus.mem[START as usize..START as usize + code.len()].copy_from_slice(code);
    let mut cpu = Cpu::new();
    cpu.regs = Registers {
        a: 0x12,
        f: 0,
        b: 0x01,
        c: 0x02,
        d: 0x03,
        e: 0x04,
        h: 0xC0,
        l: 0x00,
        sp: 0xD000,
        pc: START,
    };
    (cpu, bus)
}

/// Run one step and return the number of M-cycles it took.
fn step_cycles(cpu: &mut Cpu, bus: &mut TestBus) -> usize {
    let before = bus.cycles();
    cpu.step(bus);
    bus.cycles() - before
}

/// Execute `code` for `steps` steps, returning the final CPU.
fn run(code: &[u8], steps: usize, tweak: impl FnOnce(&mut Cpu, &mut TestBus)) -> (Cpu, TestBus) {
    let (mut cpu, mut bus) = setup(code);
    tweak(&mut cpu, &mut bus);
    for _ in 0..steps {
        cpu.step(&mut bus);
    }
    (cpu, bus)
}

// ---- Per-opcode cycle counts ----

/// M-cycles per base opcode with conditional branches NOT taken; 0 = illegal.
/// 0xCB is the prefix (covered by `cb_opcode_cycle_counts`). Transcribed from the
/// gbdev opcode table (T-cycles / 4).
#[rustfmt::skip]
const BASE_CYCLES: [u8; 256] = [
    // 0  1  2  3  4  5  6  7   8  9  A  B  C  D  E  F
       1, 3, 2, 2, 1, 1, 2, 1,  5, 2, 2, 2, 1, 1, 2, 1, // 0x
       1, 3, 2, 2, 1, 1, 2, 1,  3, 2, 2, 2, 1, 1, 2, 1, // 1x
       2, 3, 2, 2, 1, 1, 2, 1,  2, 2, 2, 2, 1, 1, 2, 1, // 2x
       2, 3, 2, 2, 3, 3, 3, 1,  2, 2, 2, 2, 1, 1, 2, 1, // 3x
       1, 1, 1, 1, 1, 1, 2, 1,  1, 1, 1, 1, 1, 1, 2, 1, // 4x
       1, 1, 1, 1, 1, 1, 2, 1,  1, 1, 1, 1, 1, 1, 2, 1, // 5x
       1, 1, 1, 1, 1, 1, 2, 1,  1, 1, 1, 1, 1, 1, 2, 1, // 6x
       2, 2, 2, 2, 2, 2, 1, 2,  1, 1, 1, 1, 1, 1, 2, 1, // 7x
       1, 1, 1, 1, 1, 1, 2, 1,  1, 1, 1, 1, 1, 1, 2, 1, // 8x
       1, 1, 1, 1, 1, 1, 2, 1,  1, 1, 1, 1, 1, 1, 2, 1, // 9x
       1, 1, 1, 1, 1, 1, 2, 1,  1, 1, 1, 1, 1, 1, 2, 1, // Ax
       1, 1, 1, 1, 1, 1, 2, 1,  1, 1, 1, 1, 1, 1, 2, 1, // Bx
       2, 3, 3, 4, 3, 4, 2, 4,  2, 4, 3, 1, 3, 6, 2, 4, // Cx
       2, 3, 3, 0, 3, 4, 2, 4,  2, 4, 3, 0, 3, 0, 2, 4, // Dx
       3, 3, 2, 0, 0, 4, 2, 4,  4, 1, 4, 0, 0, 0, 2, 4, // Ex
       3, 3, 2, 1, 0, 4, 2, 4,  3, 2, 4, 1, 0, 0, 2, 4, // Fx
];

/// Extra M-cycles when a conditional branch is taken (JR +1, JP +1, CALL +3,
/// RET +3), else `None`.
fn taken_extra(op: u8) -> Option<usize> {
    match op {
        0x20 | 0x28 | 0x30 | 0x38 => Some(1),
        0xC2 | 0xCA | 0xD2 | 0xDA => Some(1),
        0xC4 | 0xCC | 0xD4 | 0xDC => Some(3),
        0xC0 | 0xC8 | 0xD0 | 0xD8 => Some(3),
        _ => None,
    }
}

fn is_illegal(op: u8) -> bool {
    matches!(
        op,
        0xD3 | 0xDB | 0xDD | 0xE3 | 0xE4 | 0xEB | 0xEC | 0xED | 0xF4 | 0xFC | 0xFD
    )
}

#[test]
fn base_opcode_cycle_counts() {
    for op in 0..=255u8 {
        if op == 0xCB || is_illegal(op) {
            continue;
        }
        // F=0x00 takes NZ/NC; F=0xF0 takes Z/C: both branch outcomes covered.
        for f in [0x00u8, 0xF0] {
            // Operand bytes 00 03 form a valid target (0x0300) / zero JR offset;
            // the stack holds the same word for RET.
            let (mut cpu, mut bus) = setup(&[op, 0x00, 0x03]);
            bus.mem[0xD000] = 0x00;
            bus.mem[0xD001] = 0x03;
            cpu.regs.f = f;
            let taken = taken_extra(op).is_some() && {
                let cond = (op >> 3) & 3;
                let z = f & 0x80 != 0;
                let c = f & 0x10 != 0;
                match cond {
                    0 => !z,
                    1 => z,
                    2 => !c,
                    _ => c,
                }
            };
            let expected = BASE_CYCLES[op as usize] as usize
                + if taken { taken_extra(op).unwrap() } else { 0 };
            let got = step_cycles(&mut cpu, &mut bus);
            assert_eq!(got, expected, "opcode {op:#04x} f={f:#04x} taken={taken}");
        }
    }
}

#[test]
fn cb_opcode_cycle_counts() {
    for op in 0..=255u8 {
        let (mut cpu, mut bus) = setup(&[0xCB, op]);
        let expected = if op & 7 != 6 {
            2
        } else if (0x40..0x80).contains(&op) {
            3 // BIT n,(HL): read only
        } else {
            4 // read-modify-write
        };
        assert_eq!(step_cycles(&mut cpu, &mut bus), expected, "CB {op:#04x}");
    }
}

#[test]
fn illegal_opcodes_lock_the_cpu() {
    for op in (0..=255u8).filter(|&o| is_illegal(o)) {
        let (mut cpu, mut bus) = setup(&[op]);
        assert_eq!(step_cycles(&mut cpu, &mut bus), 1);
        assert!(cpu.is_locked(), "{op:#04x}");
        let pc = cpu.regs.pc;
        assert_eq!(step_cycles(&mut cpu, &mut bus), 1, "locked steps tick");
        assert_eq!(cpu.regs.pc, pc);
    }
}

// ---- Position of memory accesses within an instruction ----

#[test]
fn call_bus_sequence() {
    let (mut cpu, mut bus) = setup(&[0xCD, 0x34, 0x12]);
    cpu.step(&mut bus);
    assert_eq!(
        bus.log,
        [
            Ev::Read(0x200),
            Ev::Read(0x201),
            Ev::Read(0x202),
            Ev::Tick,
            Ev::Write(0xCFFF, 0x02),
            Ev::Write(0xCFFE, 0x03),
        ]
    );
    assert_eq!((cpu.regs.pc, cpu.regs.sp), (0x1234, 0xCFFE));
}

#[test]
fn push_has_internal_cycle_before_writes() {
    let (mut cpu, mut bus) = setup(&[0xC5]);
    cpu.step(&mut bus);
    assert_eq!(
        bus.log,
        [
            Ev::Read(0x200),
            Ev::Tick,
            Ev::Write(0xCFFF, 0x01),
            Ev::Write(0xCFFE, 0x02),
        ]
    );
}

#[test]
fn ret_variants_bus_sequence() {
    // RET: fetch, lo, hi, internal.
    let (mut cpu, mut bus) = setup(&[0xC9]);
    bus.mem[0xD000] = 0x78;
    bus.mem[0xD001] = 0x56;
    cpu.step(&mut bus);
    assert_eq!(
        bus.log,
        [
            Ev::Read(0x200),
            Ev::Read(0xD000),
            Ev::Read(0xD001),
            Ev::Tick
        ]
    );
    assert_eq!(cpu.regs.pc, 0x5678);
    assert!(!cpu.ime());

    // RET cc taken: fetch, internal (condition), lo, hi, internal.
    let (mut cpu, mut bus) = setup(&[0xC0]); // RET NZ, Z clear
    cpu.step(&mut bus);
    assert_eq!(
        bus.log,
        [
            Ev::Read(0x200),
            Ev::Tick,
            Ev::Read(0xD000),
            Ev::Read(0xD001),
            Ev::Tick
        ]
    );

    // RET cc not taken: fetch, internal.
    let (mut cpu, mut bus) = setup(&[0xC8]); // RET Z, Z clear
    cpu.step(&mut bus);
    assert_eq!(bus.log, [Ev::Read(0x200), Ev::Tick]);

    // RETI enables IME immediately.
    let (mut cpu, mut bus) = setup(&[0xD9]);
    cpu.step(&mut bus);
    assert!(cpu.ime());
    assert_eq!(bus.cycles(), 4);
}

#[test]
fn internal_cycles_of_16bit_ops() {
    let (mut cpu, mut bus) = setup(&[0x03]); // INC BC
    cpu.step(&mut bus);
    assert_eq!(bus.log, [Ev::Read(0x200), Ev::Tick]);
    let (mut cpu, mut bus) = setup(&[0xF9]); // LD SP,HL
    cpu.step(&mut bus);
    assert_eq!(bus.log, [Ev::Read(0x200), Ev::Tick]);
    assert_eq!(cpu.regs.sp, 0xC000);
    let (mut cpu, mut bus) = setup(&[0x09]); // ADD HL,BC
    cpu.step(&mut bus);
    assert_eq!(bus.log, [Ev::Read(0x200), Ev::Tick]);
    let (mut cpu, mut bus) = setup(&[0xE8, 0x01]); // ADD SP,e
    cpu.step(&mut bus);
    assert_eq!(
        bus.log,
        [Ev::Read(0x200), Ev::Read(0x201), Ev::Tick, Ev::Tick]
    );
    let (mut cpu, mut bus) = setup(&[0xF8, 0x01]); // LD HL,SP+e
    cpu.step(&mut bus);
    assert_eq!(bus.log, [Ev::Read(0x200), Ev::Read(0x201), Ev::Tick]);
}

#[test]
fn ld_a16_sp_writes_low_then_high() {
    let (mut cpu, mut bus) = setup(&[0x08, 0x00, 0xC1]);
    cpu.regs.sp = 0xBEEF;
    cpu.step(&mut bus);
    assert_eq!(
        &bus.log[3..],
        [Ev::Write(0xC100, 0xEF), Ev::Write(0xC101, 0xBE)]
    );
}

// ---- Flags ----

#[test]
fn daa_cases() {
    // (a, flags, expected a, expected flags)
    let cases: [(u8, u8, u8, u8); 6] = [
        (0x0A, 0x00, 0x10, 0x00),
        (0x7D, 0x00, 0x83, 0x00),
        (0x9A, 0x00, 0x00, 0x90),        // Z | C
        (0x0F, 0x40 | 0x20, 0x09, 0x40), // after SUB with half-borrow
        (0x00, 0x40 | 0x10, 0xA0, 0x50), // after SUB with borrow
        (0x00, 0x40, 0x00, 0xC0),
    ];
    for (a, f, ea, ef) in cases {
        let (cpu, _) = run(&[0x27], 1, |c, _| {
            c.regs.a = a;
            c.regs.f = f;
        });
        assert_eq!(
            (cpu.regs.a, cpu.regs.f),
            (ea, ef),
            "DAA a={a:#04x} f={f:#04x}"
        );
    }
}

#[test]
fn adc_sbc_half_carry() {
    // ADC A,n : 0x0F + 0x00 + carry => H
    let (cpu, _) = run(&[0xCE, 0x00], 1, |c, _| {
        c.regs.a = 0x0F;
        c.regs.f = FLAG_C;
    });
    assert_eq!((cpu.regs.a, cpu.regs.f), (0x10, FLAG_H));
    // ADC with carry-in producing carry-out and zero.
    let (cpu, _) = run(&[0xCE, 0xFF], 1, |c, _| {
        c.regs.a = 0x00;
        c.regs.f = FLAG_C;
    });
    assert_eq!((cpu.regs.a, cpu.regs.f), (0x00, FLAG_Z | FLAG_H | FLAG_C));
    // SBC A,n : 0x10 - 0x0F - 1 = 0 with half borrow.
    let (cpu, _) = run(&[0xDE, 0x0F], 1, |c, _| {
        c.regs.a = 0x10;
        c.regs.f = FLAG_C;
    });
    assert_eq!((cpu.regs.a, cpu.regs.f), (0x00, FLAG_Z | FLAG_N | FLAG_H));
    // SBC A,n : 0 - 0xFF - 1 = 0 with H and C.
    let (cpu, _) = run(&[0xDE, 0xFF], 1, |c, _| {
        c.regs.a = 0x00;
        c.regs.f = FLAG_C;
    });
    assert_eq!(
        (cpu.regs.a, cpu.regs.f),
        (0x00, FLAG_Z | FLAG_N | FLAG_H | FLAG_C)
    );
}

#[test]
fn cp_does_not_store() {
    let (cpu, _) = run(&[0xFE, 0x12], 1, |_, _| {});
    assert_eq!((cpu.regs.a, cpu.regs.f), (0x12, FLAG_Z | FLAG_N));
}

#[test]
fn add_sp_e_and_ld_hl_sp_e_flags_from_low_byte() {
    // (sp, e, expected sp, expected f)
    let cases: [(u16, u8, u16, u8); 4] = [
        (0x00FF, 0x01, 0x0100, FLAG_H | FLAG_C),
        (0x0001, 0xFF, 0x0000, FLAG_H | FLAG_C), // -1: low byte add carries
        (0x0100, 0xFF, 0x00FF, 0),               // -1: no carry from the low byte
        (0xFFF8, 0x08, 0x0000, FLAG_H | FLAG_C),
    ];
    for (sp, e, esp, ef) in cases {
        let (cpu, _) = run(&[0xE8, e], 1, |c, _| c.regs.sp = sp);
        assert_eq!(
            (cpu.regs.sp, cpu.regs.f),
            (esp, ef),
            "ADD SP sp={sp:#06x} e={e:#04x}"
        );
        let (cpu, _) = run(&[0xF8, e], 1, |c, _| c.regs.sp = sp);
        assert_eq!(
            (cpu.hl(), cpu.regs.sp, cpu.regs.f),
            (esp, sp, ef),
            "LD HL,SP+e sp={sp:#06x} e={e:#04x}"
        );
    }
}

#[test]
fn add_hl_flags() {
    let (cpu, _) = run(&[0x09], 1, |c, _| {
        c.regs.h = 0x0F;
        c.regs.l = 0xFF;
        c.regs.b = 0x00;
        c.regs.c = 0x01;
        c.regs.f = FLAG_Z | FLAG_N;
    });
    assert_eq!(cpu.hl(), 0x1000);
    assert_eq!(cpu.regs.f, FLAG_Z | FLAG_H); // Z kept, N cleared, H set
    let (cpu, _) = run(&[0x29], 1, |c, _| {
        c.regs.h = 0x80;
        c.regs.l = 0x00;
    });
    assert_eq!((cpu.hl(), cpu.regs.f), (0x0000, FLAG_C));
}

#[test]
fn inc_dec_flags_keep_carry() {
    let (cpu, _) = run(&[0x3C], 1, |c, _| {
        c.regs.a = 0x0F;
        c.regs.f = FLAG_C;
    });
    assert_eq!((cpu.regs.a, cpu.regs.f), (0x10, FLAG_H | FLAG_C));
    let (cpu, _) = run(&[0x3C], 1, |c, _| c.regs.a = 0xFF);
    assert_eq!((cpu.regs.a, cpu.regs.f), (0x00, FLAG_Z | FLAG_H));
    let (cpu, _) = run(&[0x3D], 1, |c, _| c.regs.a = 0x10);
    assert_eq!((cpu.regs.a, cpu.regs.f), (0x0F, FLAG_N | FLAG_H));
    // INC (HL) read-modify-write through memory.
    let (_, bus) = run(&[0x34], 1, |_, b| b.mem[0xC000] = 0x41);
    assert_eq!(bus.mem[0xC000], 0x42);
}

#[test]
fn rotates_and_shifts() {
    // RLCA never sets Z.
    let (cpu, _) = run(&[0x07], 1, |c, _| c.regs.a = 0x80);
    assert_eq!((cpu.regs.a, cpu.regs.f), (0x01, FLAG_C));
    let (cpu, _) = run(&[0x17], 1, |c, _| {
        c.regs.a = 0x00;
        c.regs.f = FLAG_Z;
    });
    assert_eq!((cpu.regs.a, cpu.regs.f), (0x00, 0));
    // RRA pulls carry into bit 7.
    let (cpu, _) = run(&[0x1F], 1, |c, _| {
        c.regs.a = 0x01;
        c.regs.f = FLAG_C;
    });
    assert_eq!((cpu.regs.a, cpu.regs.f), (0x80, FLAG_C));
    // CB versions set Z from the result.
    let (cpu, _) = run(&[0xCB, 0x00], 1, |c, _| c.regs.b = 0);
    assert_eq!((cpu.regs.b, cpu.regs.f), (0, FLAG_Z));
    // SRA keeps the sign bit; SRL does not; SWAP swaps nibbles and clears C.
    let (cpu, _) = run(&[0xCB, 0x28], 1, |c, _| c.regs.b = 0x81);
    assert_eq!((cpu.regs.b, cpu.regs.f), (0xC0, FLAG_C));
    let (cpu, _) = run(&[0xCB, 0x38], 1, |c, _| c.regs.b = 0x81);
    assert_eq!((cpu.regs.b, cpu.regs.f), (0x40, FLAG_C));
    let (cpu, _) = run(&[0xCB, 0x30], 1, |c, _| {
        c.regs.b = 0xF0;
        c.regs.f = FLAG_C;
    });
    assert_eq!((cpu.regs.b, cpu.regs.f), (0x0F, 0));
}

#[test]
fn bit_res_set() {
    let (cpu, _) = run(&[0xCB, 0x78], 1, |c, _| {
        c.regs.b = 0x7F; // BIT 7,B
        c.regs.f = FLAG_C | FLAG_N;
    });
    assert_eq!(cpu.regs.f, FLAG_Z | FLAG_H | FLAG_C);
    let (cpu, _) = run(&[0xCB, 0x80, 0xCB, 0xC8], 2, |_, _| {}); // RES 0,B ; SET 1,B
    assert_eq!(cpu.regs.b, 0x02);
}

#[test]
fn pop_af_masks_low_nibble() {
    let (cpu, _) = run(&[0xF1], 1, |_, b| {
        b.mem[0xD000] = 0xFF;
        b.mem[0xD001] = 0x34;
    });
    assert_eq!((cpu.regs.a, cpu.regs.f), (0x34, 0xF0));
}

#[test]
fn ld_hl_inc_dec_and_jr_negative() {
    let (cpu, bus) = run(&[0x22, 0x32], 2, |_, _| {}); // LD (HL+),A ; LD (HL-),A
    assert_eq!(cpu.hl(), 0xC000);
    assert_eq!(bus.mem[0xC000], 0x12);
    assert_eq!(bus.mem[0xC001], 0x12);
    let (cpu, _) = run(&[0x18, 0xFE], 1, |_, _| {}); // JR -2 => loops to itself
    assert_eq!(cpu.regs.pc, START);
}

#[test]
fn ld_b_b_sets_breakpoint_flag_once() {
    let (mut cpu, mut bus) = setup(&[0x40]);
    cpu.step(&mut bus);
    assert!(cpu.take_ld_b_b_breakpoint());
    assert!(!cpu.take_ld_b_b_breakpoint());
}

// ---- Interrupts, EI/DI, HALT, STOP ----

#[test]
fn ei_takes_effect_after_following_instruction() {
    let (mut cpu, mut bus) = setup(&[0xFB, 0x00, 0x00]); // EI ; NOP ; NOP
    bus.ie = 0x01;
    bus.iflag = 0x01;
    cpu.step(&mut bus); // EI
    assert!(!cpu.ime());
    cpu.step(&mut bus); // NOP runs before the interrupt is taken
    assert_eq!(cpu.regs.pc, START + 2);
    assert!(cpu.ime());
    assert_eq!(step_cycles(&mut cpu, &mut bus), 5); // dispatch
    assert_eq!(cpu.regs.pc, 0x0040);
    assert_eq!(cpu.regs.sp, 0xCFFE);
    assert_eq!(bus.mem[0xCFFE], (START + 2) as u8);
    assert_eq!(bus.mem[0xCFFF], ((START + 2) >> 8) as u8);
    assert_eq!(bus.iflag, 0);
    assert!(!cpu.ime());
}

#[test]
fn di_cancels_pending_ei() {
    let (mut cpu, mut bus) = setup(&[0xFB, 0xF3, 0x00, 0x00]); // EI ; DI ; NOP
    bus.ie = 0x01;
    bus.iflag = 0x01;
    for _ in 0..3 {
        cpu.step(&mut bus);
    }
    assert!(!cpu.ime());
    assert_eq!(cpu.regs.pc, START + 3);
}

#[test]
fn dispatch_cancels_a_pending_ei() {
    // EI with IME already on, an interrupt arriving while EI runs: the
    // dispatch must clear the stale `ei_pending`, or the first handler
    // instruction would turn IME back on and allow nesting.
    let (mut cpu, mut bus) = setup(&[0xFB, 0x00]); // EI ; NOP
    cpu.ime = true;
    bus.ie = 0x01;
    cpu.step(&mut bus); // EI (nothing pending yet)
    assert!(cpu.ei_pending);
    bus.iflag = 0x01; // raised during EI's last M-cycle
    assert_eq!(step_cycles(&mut cpu, &mut bus), 5); // dispatch
    assert_eq!(cpu.regs.pc, 0x0040);
    assert!(!cpu.ime());
    assert!(!cpu.ei_pending);
    cpu.step(&mut bus); // first handler instruction (NOP at 0x0040)
    assert!(!cpu.ime(), "IME stays off inside the handler");
}

#[test]
fn ei_ei_enables_after_second() {
    let (mut cpu, mut bus) = setup(&[0xFB, 0xFB, 0x00]);
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert!(cpu.ime());
}

#[test]
fn interrupt_priority_and_vector() {
    let (mut cpu, mut bus) = setup(&[0x00]);
    cpu.ime = true;
    bus.ie = 0x1F;
    bus.iflag = 0x14; // Timer + Joypad
    assert_eq!(step_cycles(&mut cpu, &mut bus), 5);
    assert_eq!(cpu.regs.pc, 0x0050);
    assert_eq!(bus.iflag, 0x10);
}

#[test]
fn dispatch_bus_sequence() {
    let (mut cpu, mut bus) = setup(&[0x00]);
    cpu.ime = true;
    bus.ie = 0x01;
    bus.iflag = 0x01;
    cpu.step(&mut bus);
    assert_eq!(
        bus.log,
        [
            Ev::Tick,
            Ev::Tick,
            Ev::Write(0xCFFF, 0x02),
            Ev::Write(0xCFFE, 0x00),
            Ev::Tick,
        ]
    );
}

#[test]
fn ie_push_quirk_jumps_to_zero_when_ie_cancelled() {
    // SP=0 so the high-byte push lands on IE (0xFFFF); PCh = 0x02 clears bit 0.
    let (mut cpu, mut bus) = setup(&[0x00]);
    cpu.ime = true;
    cpu.regs.sp = 0x0000;
    bus.ie = 0x01;
    bus.iflag = 0x01;
    cpu.step(&mut bus);
    assert_eq!(bus.ie, 0x02);
    assert_eq!(cpu.regs.pc, 0x0000);
    assert_eq!(bus.iflag, 0x01, "IF bit is not acknowledged when cancelled");
}

#[test]
fn halt_wakes_without_ime_and_continues() {
    let (mut cpu, mut bus) = setup(&[0x76, 0x04]); // HALT ; INC B
    cpu.step(&mut bus);
    assert!(cpu.is_halted());
    assert_eq!(step_cycles(&mut cpu, &mut bus), 1);
    assert!(cpu.is_halted());
    bus.ie = 0x04;
    bus.iflag = 0x04;
    cpu.step(&mut bus); // wakes, executes INC B (IME=0: no dispatch)
    assert!(!cpu.is_halted());
    assert_eq!(cpu.regs.b, 0x02);
    assert_eq!(bus.iflag, 0x04, "IF not cleared without dispatch");
}

#[test]
fn halt_with_ime_dispatches_on_wake() {
    let (mut cpu, mut bus) = setup(&[0x76, 0x00]);
    cpu.ime = true;
    cpu.step(&mut bus);
    assert!(cpu.is_halted());
    bus.ie = 0x01;
    bus.iflag = 0x01;
    cpu.step(&mut bus);
    assert_eq!(cpu.regs.pc, 0x0040);
    assert_eq!(bus.mem[0xCFFE], (START + 1) as u8); // return address after HALT
}

#[test]
fn halt_bug_reads_next_byte_twice() {
    let (mut cpu, mut bus) = setup(&[0x76, 0x04, 0x00]); // HALT ; INC B
    bus.ie = 0x01;
    bus.iflag = 0x01;
    cpu.step(&mut bus);
    assert!(!cpu.is_halted(), "HALT bug: no halt");
    cpu.step(&mut bus);
    assert_eq!((cpu.regs.b, cpu.regs.pc), (0x02, START + 1));
    cpu.step(&mut bus);
    assert_eq!((cpu.regs.b, cpu.regs.pc), (0x03, START + 2));
}

#[test]
fn stop_calls_bus_and_idles() {
    let (mut cpu, mut bus) = setup(&[0x10, 0x00, 0x04]);
    cpu.step(&mut bus);
    assert!(bus.stopped);
    assert_eq!(cpu.regs.pc, START + 2);
    assert!(cpu.is_halted());
    assert_eq!(step_cycles(&mut cpu, &mut bus), 1);
    assert_eq!(cpu.regs.pc, START + 2);
}

#[test]
fn stop_wakes_on_joypad_line_with_ie_zero() {
    // STOP; INC B. A pending, enabled interrupt must not wake it either.
    let (mut cpu, mut bus) = setup(&[0x10, 0x00, 0x04]);
    cpu.step(&mut bus);
    bus.ie = 0x01;
    bus.iflag = 0x01;
    for _ in 0..10 {
        cpu.step(&mut bus);
    }
    assert!(cpu.is_halted());
    assert_eq!(cpu.regs.pc, START + 2);
    bus.joypad_low = true;
    cpu.step(&mut bus);
    assert!(!cpu.is_halted());
    assert_eq!(cpu.regs.pc, START + 3, "INC B executed after waking");
}

#[test]
fn reti_returns_and_enables_ime() {
    let (mut cpu, mut bus) = setup(&[0xD9]);
    bus.mem[0xD000] = 0x00;
    bus.mem[0xD001] = 0x03;
    cpu.step(&mut bus);
    assert_eq!((cpu.regs.pc, cpu.regs.sp), (0x0300, 0xD002));
    assert!(cpu.ime());
}

#[test]
fn rst_pushes_return_address() {
    let (mut cpu, mut bus) = setup(&[0xEF]); // RST 28
    cpu.step(&mut bus);
    assert_eq!(cpu.regs.pc, 0x0028);
    assert_eq!(
        bus.log,
        [
            Ev::Read(START),
            Ev::Tick,
            Ev::Write(0xCFFF, 0x02),
            Ev::Write(0xCFFE, 0x01),
        ]
    );
}

#[test]
fn ei_halt_with_pending_interrupt_returns_to_halt() {
    // EI; HALT with VBlank pending: serviced right after HALT, return address
    // is the HALT itself, and the ISR's first fetch is a normal one.
    let (mut cpu, mut bus) = setup(&[0xFB, 0x76, 0x04]);
    bus.ie = 0x01;
    bus.iflag = 0x01;
    bus.mem[0x40] = 0x0C; // INC C
    bus.mem[0x41] = 0x0C; // INC C
    cpu.step(&mut bus); // EI
    cpu.step(&mut bus); // HALT (bug condition: IME still 0)
    cpu.step(&mut bus); // dispatch
    assert_eq!(cpu.regs.pc, 0x0040);
    let ret = u16::from(bus.mem[0xCFFE]) | (u16::from(bus.mem[0xCFFF]) << 8);
    assert_eq!(ret, START + 1, "returns to the HALT");
    let c = cpu.regs.c;
    cpu.step(&mut bus);
    assert_eq!(
        (cpu.regs.c, cpu.regs.pc),
        (c + 1, 0x0041),
        "ISR fetch not doubled"
    );
}
