//! Task 2: two machines built from the same ROM and fed the same input script
//! must be bit-identical every frame (framebuffer, audio, battery RAM, CPU
//! state, cycle count, serial output). Also: observation order (when audio is
//! drained) and power-cycling via `load_rom` must not change the result.

mod common;

use common::*;
use gb_core::{GameBoy, Registers};

#[derive(Debug, PartialEq, Eq)]
struct Digest {
    frame: usize,
    fb: u64,
    audio: u64,
    audio_len: usize,
    battery: Option<u64>,
    regs: [u16; 6],
    mcycles: u64,
    serial: u64,
    locked: bool,
}

fn regs(r: Registers) -> [u16; 6] {
    [
        u16::from(r.a) << 8 | u16::from(r.f),
        u16::from(r.b) << 8 | u16::from(r.c),
        u16::from(r.d) << 8 | u16::from(r.e),
        u16::from(r.h) << 8 | u16::from(r.l),
        r.sp,
        r.pc,
    ]
}

fn digest(gb: &mut GameBoy, frame: usize) -> Digest {
    let audio = gb.audio_samples();
    Digest {
        frame,
        fb: hash_bytes(gb.framebuffer()),
        audio: hash_f32s(&audio),
        audio_len: audio.len(),
        battery: gb.battery_ram().map(|b| hash_bytes(&b)),
        regs: regs(gb.cpu_registers()),
        mcycles: gb.mcycles(),
        serial: hash_bytes(gb.serial_output()),
        locked: gb.cpu_locked(),
    }
}

/// Run `rom` twice in lockstep and compare after every frame. `None` if the
/// ROM does not load.
fn assert_lockstep(label: &str, rom: &[u8], script: &InputScript) -> Option<()> {
    let mut a = GameBoy::new(rom.to_vec()).ok()?;
    let mut b = GameBoy::new(rom.to_vec()).ok()?;
    for (frame, events) in script.iter().enumerate() {
        apply_inputs(&mut a, events);
        apply_inputs(&mut b, events);
        a.step_frame();
        b.step_frame();
        let (da, db) = (digest(&mut a, frame), digest(&mut b, frame));
        assert_eq!(da, db, "{label}: diverged at frame {frame}");
    }
    Some(())
}

#[test]
fn random_roms_are_deterministic() {
    let mut ran = 0;
    for seed in 0..60u64 {
        let mut rng = Rng::new(0xDE7E ^ seed);
        let rom = random_rom(&mut rng);
        let script = random_input_script(&mut rng, 12);
        if assert_lockstep(&format!("random seed {seed}"), &rom, &script).is_some() {
            ran += 1;
        }
    }
    assert!(ran >= 15, "only {ran} random ROMs loaded");
}

/// Programs with real behaviour (RAM writes, APU, timers) on every mapper.
#[test]
fn generated_programs_are_deterministic() {
    for (i, cart_type) in VALID_CART_TYPES.into_iter().enumerate() {
        let mut rng = Rng::new(0x9E0 + i as u64);
        let mut p = Program::default();
        p.write(0x0000, 0x0A); // RAM enable
        p.write(0xFF26, 0x80)
            .write(0xFF24, 0x77)
            .write(0xFF25, 0xFF);
        for _ in 0..200 {
            p.write(0xA000 + rng.below(0x2000) as u16, rng.u8());
            p.write(0xFF10 + rng.below(0x20) as u16, rng.u8());
            p.delay(rng.u8());
        }
        p.write(0xFF07, 0x05).write(0xFFFF, 0x04).ei().spin();
        let rom = make_rom(0x10000, cart_type, 1, 0x03, &p.0);
        let script = random_input_script(&mut rng, 40);
        assert_lockstep(&format!("type {cart_type:#04x}"), &rom, &script)
            .unwrap_or_else(|| panic!("type {cart_type:#04x} failed to load"));
    }
}

/// When the audio buffer is drained, and the output sample rate, are
/// observation details: they must not change emulation.
#[test]
fn observation_does_not_change_emulation() {
    let mut rng = Rng::new(0x0B5);
    let mut p = Program::default();
    p.write(0xFF26, 0x80)
        .write(0xFF24, 0x77)
        .write(0xFF25, 0xFF);
    p.write(0xFF12, 0xF0)
        .write(0xFF13, 0x80)
        .write(0xFF14, 0x87);
    p.write(0xFF17, 0xA3)
        .write(0xFF18, 0x40)
        .write(0xFF19, 0x86);
    p.spin();
    let rom = make_rom(0x8000, 0x00, 0, 0, &p.0);
    let script = random_input_script(&mut rng, 63);

    let mut every = GameBoy::new(rom.clone()).unwrap();
    let mut lazy = GameBoy::new(rom.clone()).unwrap();
    let mut other_rate = GameBoy::new(rom).unwrap();
    other_rate.set_audio_sample_rate(22_050);
    let (mut audio_every, mut audio_lazy) = (Vec::new(), Vec::new());
    for (frame, events) in script.iter().enumerate() {
        for gb in [&mut every, &mut lazy, &mut other_rate] {
            apply_inputs(gb, events);
            gb.step_frame();
        }
        audio_every.extend(every.audio_samples());
        if frame % 7 == 6 {
            audio_lazy.extend(lazy.audio_samples());
        }
        assert_eq!(
            every.framebuffer()[..],
            lazy.framebuffer()[..],
            "frame {frame}"
        );
        assert_eq!(
            every.framebuffer()[..],
            other_rate.framebuffer()[..],
            "frame {frame}"
        );
        assert_eq!(every.mcycles(), other_rate.mcycles());
        assert_eq!(
            regs(every.cpu_registers()),
            regs(other_rate.cpu_registers())
        );
    }
    audio_lazy.extend(lazy.audio_samples());
    assert!(
        audio_every.iter().any(|&s| s != 0.0),
        "ROM produced silence"
    );
    assert_eq!(audio_every.len(), audio_lazy.len());
    assert_eq!(hash_f32s(&audio_every), hash_f32s(&audio_lazy));
}

/// `load_rom` on a used machine is a full power cycle.
#[test]
fn load_rom_equals_fresh_machine() {
    for seed in 0..10u64 {
        let mut rng = Rng::new(0x10AD + seed);
        let (r1, r2) = loop {
            let (a, b) = (random_rom(&mut rng), random_rom(&mut rng));
            if GameBoy::new(a.clone()).is_ok() && GameBoy::new(b.clone()).is_ok() {
                break (a, b);
            }
        };
        let script = random_input_script(&mut rng, 10);
        let mut used = GameBoy::new(r1).unwrap();
        for events in &script {
            apply_inputs(&mut used, events);
            used.step_frame();
        }
        // Held buttons from the previous session must not leak across.
        used.load_rom(r2.clone()).unwrap();
        let mut fresh = GameBoy::new(r2).unwrap();
        for (frame, events) in script.iter().enumerate() {
            apply_inputs(&mut used, events);
            apply_inputs(&mut fresh, events);
            used.step_frame();
            fresh.step_frame();
            assert_eq!(
                digest(&mut used, frame),
                digest(&mut fresh, frame),
                "seed {seed}: reload diverged at frame {frame}"
            );
        }
    }
}

/// The real homebrew / test / corpus ROMs, when `GB_ROMS_DIR` is set.
#[test]
fn real_roms_are_deterministic() {
    let Some(dir) = roms_dir() else {
        return; // quietly skipped
    };
    let roms = find_roms(&dir);
    let mut ran = 0;
    for (i, path) in roms.iter().enumerate() {
        let Ok(rom) = std::fs::read(path) else {
            continue;
        };
        let mut rng = Rng::new(0x5EA1 + i as u64);
        let script = random_input_script(&mut rng, 20);
        if assert_lockstep(&path.display().to_string(), &rom, &script).is_some() {
            ran += 1;
        }
    }
    eprintln!("determinism: {ran}/{} real ROMs compared", roms.len());
}
