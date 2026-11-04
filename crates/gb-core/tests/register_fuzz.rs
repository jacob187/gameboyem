//! Task 5: bulk register fuzz. A generated ROM program writes random values to
//! every IO register (FF00-FF7F, FFFF), VRAM, OAM, WRAM, echo RAM and HRAM
//! (plus cartridge RAM and unusable/read-only areas) and interleaves reads and
//! delay loops. The machine must not panic and must keep producing frames.

mod common;

use common::*;
use gb_core::{GameBoy, M_CYCLES_PER_FRAME};

const DMA_REG: u16 = 0xFF46;
const DMA_ROUTINE: u16 = 0xFF80;
/// `LDH (46),A ; LD A,40 ; loop: DEC A ; JR NZ,loop ; RET` : start OAM DMA from
/// page A and busy-wait the 160 M-cycles from HRAM, as real games do.
const DMA_CODE: [u8; 8] = [0xE0, 0x46, 0x3E, 0x28, 0x3D, 0x20, 0xFD, 0xC9];

/// Copy the DMA routine into HRAM.
fn install_dma_routine(p: &mut Program) {
    for (i, &b) in DMA_CODE.iter().enumerate() {
        p.write(DMA_ROUTINE + i as u16, b);
    }
}

/// Run an OAM DMA from `page` (interrupts off for its duration).
fn dma_call(p: &mut Program, page: u8) {
    // DI ; LD A,page ; CALL FF80
    p.0.extend_from_slice(&[0xF3, 0x3E, page, 0xCD, 0x80, 0xFF]);
}

/// Address pools covering every region the task names.
fn random_addr(rng: &mut Rng) -> u16 {
    let addr = match rng.below(12) {
        0..=3 => 0xFF00 + rng.below(0x80) as u16, // IO registers
        4 => 0xFFFF,                              // IE
        5 => 0x8000 + rng.below(0x2000) as u16,   // VRAM
        6 => 0xFE00 + rng.below(0x100) as u16,    // OAM + unusable
        7 => 0xC000 + rng.below(0x2000) as u16,   // WRAM
        8 => 0xE000 + rng.below(0x1E00) as u16,   // echo RAM
        // HRAM, except the interrupt-dispatch stack slots at FFFC/FFFD.
        9 => 0xFF80 + rng.below(0x7D) as u16,
        10 => 0xA000 + rng.below(0x2000) as u16, // cartridge RAM window
        _ => rng.u16() | 0x8000,                 // anything non-ROM
    };
    match addr {
        // Keep the interrupt-dispatch stack slots intact.
        0xFFFC | 0xFFFD => 0xFFFB,
        // DMA is only started through the HRAM routine (see `dma_call`):
        // executing from ROM while OAM DMA runs reads the DMA bus (correct
        // hardware behaviour, and fatal to a ROM-resident program).
        DMA_REG => 0xFF47,
        // Keep the DMA routine itself intact.
        DMA_ROUTINE..=0xFF8F => 0xFF90,
        a => a,
    }
}

/// A write-stream program. It never writes below 0x8000, so the MBC (and hence
/// the code at 0x0150) is not disturbed. SP stays at FFFE, so interrupt
/// dispatch pushes to FFFC/FFFD: the stream avoids those two HRAM bytes
/// (clobbering a return address is a ROM bug, not an emulator one).
fn build_fuzz_program(rng: &mut Rng, ops: usize) -> Vec<u8> {
    let mut p = Program::default();
    install_dma_routine(&mut p);
    for _ in 0..ops {
        if p.len() + 40 > MAX_PROGRAM {
            break;
        }
        match rng.below(20) {
            0 => {
                p.read(random_addr(rng));
            }
            1 => {
                p.delay(rng.u8());
            }
            2 => {
                p.ei();
            }
            3 => {
                p.di();
            }
            4 => {
                dma_call(&mut p, rng.u8());
            }
            _ => {
                p.write(random_addr(rng), rng.u8());
            }
        }
    }
    // Recovery epilogue: LCD on (BG+OBJ on), interrupts handled by RETI stubs,
    // then spin. IE/IF are left in whatever state the fuzz stream chose.
    p.write(0xFF40, 0x91).ei().spin();
    p.0
}

fn run_fuzz(seed: u64, cart_type: u8, ram_code: u8, ops: usize) {
    let mut rng = Rng::new(0xF022 ^ seed);
    let prog = build_fuzz_program(&mut rng, ops);
    let rom = make_rom(0x8000, cart_type, 0, ram_code, &prog);
    let mut gb = GameBoy::new(rom).unwrap();

    for _ in 0..40 {
        let before = gb.mcycles();
        gb.step_frame();
        assert!(
            gb.mcycles() > before,
            "seed {seed}: machine did not advance"
        );
        assert!(
            gb.mcycles() - before <= STEP_FRAME_MAX_MCYCLES,
            "seed {seed}: step_frame overran its cap"
        );
        let audio = gb.audio_samples();
        assert!(
            audio
                .iter()
                .all(|s| s.is_finite() && (-1.0..=1.0).contains(s)),
            "seed {seed}: audio out of range"
        );
    }
    assert!(
        !gb.cpu_locked(),
        "seed {seed}: CPU locked {:x?}",
        gb.cpu_registers()
    );

    // The machine must keep producing frames: a full frame per step_frame
    // (tolerance: the instruction crossing the frame boundary), unless the
    // fuzz left the LCD off. The epilogue wrote LCDC=0x91, but later stream
    // bytes can't run again, so the LCD must be on and frames must flow.
    let mut full = 0;
    for _ in 0..5 {
        let before = gb.mcycles();
        gb.step_frame();
        let spent = gb.mcycles() - before;
        if spent.abs_diff(M_CYCLES_PER_FRAME as u64) <= 12 {
            full += 1;
        }
        assert!(gb.framebuffer().iter().all(|&s| s <= 3));
    }
    assert!(
        full >= 4,
        "seed {seed}: only {full}/5 regular frames after fuzz"
    );
}

#[test]
fn register_write_stream_never_panics() {
    for seed in 0..48 * fuzz_scale() {
        run_fuzz(seed, 0x00, 0, 400);
    }
}

#[test]
fn register_write_stream_with_mbc_ram() {
    // Cartridge-RAM window writes hit real RAM here; also covers disabled RAM.
    for (i, cart_type) in [0x03u8, 0x13, 0x1B, 0x10, 0x09].into_iter().enumerate() {
        for seed in 0..5 * fuzz_scale() {
            run_fuzz(1000 + 100 * i as u64 + seed, cart_type, 0x03, 300);
        }
    }
}

/// Exhaustive sweep: every IO register with every value pattern, then frames.
#[test]
fn every_io_register_every_pattern() {
    for pattern in [0x00u8, 0xFF, 0x55, 0xAA, 0x01, 0x80, 0x7F, 0xFE] {
        let mut p = Program::default();
        install_dma_routine(&mut p);
        for addr in (0xFF00u16..=0xFF7F).chain([0xFFFF]) {
            if addr == DMA_REG {
                dma_call(&mut p, pattern);
                continue;
            }
            p.write(addr, pattern);
            p.read(addr);
        }
        p.write(0xFF40, 0x91).ei().spin();
        let mut gb = GameBoy::new(make_rom(0x8000, 0x00, 0, 0, &p.0)).unwrap();
        for _ in 0..10 {
            gb.step_frame();
        }
        assert!(!gb.cpu_locked());
    }
}

/// Fill VRAM, OAM, WRAM, echo and HRAM completely, with the LCD on and off.
#[test]
fn full_memory_fill_with_lcd_on_and_off() {
    for lcdc in [0x91u8, 0x00, 0xFF, 0x80] {
        let mut rng = Rng::new(u64::from(lcdc) + 77);
        // A loop-based filler: HL walks the range, writing a rolling value.
        // LD HL,start ; loop: LD A,E ; LD (HL+),A ; INC E ; LD A,H ; CP end_hi ; JR NZ,loop
        let mut code = Vec::new();
        code.extend_from_slice(&[0x3E, lcdc, 0xE0, 0x40]); // LD A,lcdc ; LDH (40),A
        code.extend_from_slice(&[0x1E, rng.u8()]); // LD E,seed
        for (start_hi, end_hi) in [(0x80u8, 0xA0u8), (0xC0, 0xE0), (0xE0, 0xFE), (0xFE, 0xFF)] {
            code.extend_from_slice(&[0x21, 0x00, start_hi]); // LD HL,start
            code.extend_from_slice(&[0x7B, 0x22, 0x1C, 0x7C, 0xFE, end_hi, 0x20, 0xF8]);
        }
        code.extend_from_slice(&[0xF3, 0x18, 0xFE]);
        let mut gb = GameBoy::new(make_rom(0x8000, 0x00, 0, 0, &code)).unwrap();
        for _ in 0..8 {
            gb.step_frame();
        }
        assert!(!gb.cpu_locked());
        // WRAM and its echo are the same memory.
        assert_eq!(gb.peek(0xC123), gb.peek(0xE123));
    }
}
