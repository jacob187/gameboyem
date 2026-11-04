//! Task 1: random ROM images (valid and invalid headers, every mapper) must
//! either fail to load cleanly or run for several frames under random button
//! input without panicking (debug builds also trap arithmetic overflow).

mod common;

use common::*;
use gb_core::{CartridgeError, GameBoy, SCREEN_HEIGHT, SCREEN_WIDTH};

const ROMS: u64 = 160;
const FRAMES: usize = 12;

/// Invariants that must hold after every frame of any ROM.
fn check_frame(gb: &mut GameBoy, label: &str, before: u64) {
    assert_eq!(gb.framebuffer().len(), SCREEN_WIDTH * SCREEN_HEIGHT);
    assert!(
        gb.framebuffer().iter().all(|&s| s <= 3),
        "{label}: framebuffer shade out of range"
    );
    let spent = gb.mcycles() - before;
    assert!(
        spent <= STEP_FRAME_MAX_MCYCLES,
        "{label}: step_frame ran {spent} M-cycles"
    );
    let audio = gb.audio_samples();
    assert!(
        audio.len() % 2 == 0,
        "{label}: odd interleaved sample count"
    );
    assert!(
        audio
            .iter()
            .all(|s| s.is_finite() && (-1.0..=1.0).contains(s)),
        "{label}: audio sample out of range"
    );
}

#[test]
fn random_roms_never_panic() {
    let (mut loaded, mut rejected) = (0, 0);
    for seed in 0..ROMS {
        let mut rng = Rng::new(0xA11CE ^ seed);
        let rom = random_rom(&mut rng);
        let label = format!(
            "seed {seed} (len {:#x}, type {:#04x})",
            rom.len(),
            rom.get(0x147).copied().unwrap_or(0)
        );
        let mut gb = match GameBoy::new(rom) {
            Ok(gb) => gb,
            Err(_) => {
                rejected += 1;
                continue;
            }
        };
        loaded += 1;
        // Header accessors and the battery API must be safe on any loaded cart.
        let _ = gb.cartridge_header();
        let _ = gb.battery_ram();
        let script = random_input_script(&mut rng, FRAMES);
        for events in &script {
            apply_inputs(&mut gb, events);
            let before = gb.mcycles();
            gb.step_frame();
            check_frame(&mut gb, &label, before);
        }
        let _ = gb.serial_output();
        let _ = gb.peek(rng.u16());
    }
    // The generator must actually exercise both outcomes.
    assert!(loaded >= ROMS / 4, "only {loaded} ROMs loaded");
    assert!(rejected >= ROMS / 10, "only {rejected} ROMs rejected");
}

/// Random single-byte button mashing (every button every frame, both edges).
#[test]
fn button_mashing_never_panics() {
    for seed in 0..8u64 {
        let mut rng = Rng::new(0xB077 + seed);
        let mut rom = vec![0u8; 0x8000];
        rng.fill(&mut rom[0x150..0x2000]);
        rom[0x100..0x104].copy_from_slice(&[0x00, 0xC3, 0x50, 0x01]);
        rom[0x147] = 0;
        rom[0x148] = 0;
        rom[0x149] = 0;
        let mut gb = GameBoy::new(rom).unwrap();
        for _ in 0..30 {
            for b in gb_core::Button::ALL {
                gb.set_button(b, rng.chance(1, 2));
                for _ in 0..rng.below(40) {
                    gb.step_instruction();
                }
            }
            gb.step_frame();
        }
    }
}

#[test]
fn degenerate_images_are_rejected_not_panicked() {
    assert_eq!(
        GameBoy::new(Vec::new()).err(),
        Some(CartridgeError::TooSmall(0))
    );
    for len in [1usize, 0x100, 0x14F] {
        assert!(matches!(
            GameBoy::new(vec![0; len]),
            Err(CartridgeError::TooSmall(_))
        ));
    }
    let too_big = vec![0u8; gb_core::cartridge::MAX_ROM_SIZE + 1];
    assert!(matches!(
        GameBoy::new(too_big),
        Err(CartridgeError::TooLarge(_))
    ));
    // Exactly the header area: loads (padded), and runs.
    let mut gb = GameBoy::new(make_rom(0x150, 0x00, 0, 0, &[])).unwrap();
    gb.step_frame();
}

/// `load_rom` replaces the machine, and a failed load leaves the old one usable.
#[test]
fn load_rom_failure_keeps_machine_usable() {
    let mut gb = GameBoy::new(make_rom(0x8000, 0x00, 0, 0, &[0x18, 0xFE])).unwrap();
    gb.step_frame();
    assert!(gb.load_rom(vec![0; 10]).is_err());
    gb.step_frame();
    gb.load_rom(make_rom(0x10000, 0x01, 1, 0, &[0x18, 0xFE]))
        .unwrap();
    gb.step_frame();
}

/// Every cartridge type byte, with every ROM and RAM size code.
#[test]
fn every_header_combination_loads_or_errors() {
    for cart_type in 0..=255u8 {
        for rom_code in [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0x52, 0xFF] {
            for ram_code in 0..=7u8 {
                let rom = make_rom(
                    0x8000 << (rom_code.min(4)),
                    cart_type,
                    rom_code,
                    ram_code,
                    &[0x18, 0xFE],
                );
                if let Ok(mut gb) = GameBoy::new(rom) {
                    gb.step_frame();
                    let _ = gb.battery_ram();
                }
            }
        }
    }
}
