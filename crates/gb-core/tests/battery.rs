//! Task 3: battery RAM written by a ROM survives save -> load into a fresh
//! machine -> save, byte for byte, on MBC1/MBC3/MBC5 RAM carts; malformed save
//! files are rejected or handled gracefully, never a panic.

mod common;

use common::*;
use gb_core::{CartridgeError, GameBoy};

const RAM_BANK: usize = 0x2000;
/// Length of the MBC3 RTC footer appended to the save (BGB/SameBoy layout).
const RTC_FOOTER: usize = 48;

#[derive(Clone, Copy, Debug)]
struct Case {
    cart_type: u8,
    ram_code: u8,
    banks: usize,
    rtc: bool,
}

const CASES: [Case; 12] = [
    // MBC1 + RAM + battery
    Case {
        cart_type: 0x03,
        ram_code: 0x02,
        banks: 1,
        rtc: false,
    },
    Case {
        cart_type: 0x03,
        ram_code: 0x03,
        banks: 4,
        rtc: false,
    },
    // MBC3 + RAM + battery (+ timer)
    Case {
        cart_type: 0x13,
        ram_code: 0x02,
        banks: 1,
        rtc: false,
    },
    Case {
        cart_type: 0x13,
        ram_code: 0x03,
        banks: 4,
        rtc: false,
    },
    Case {
        cart_type: 0x10,
        ram_code: 0x03,
        banks: 4,
        rtc: true,
    },
    Case {
        cart_type: 0x10,
        ram_code: 0x02,
        banks: 1,
        rtc: true,
    },
    // MBC5 + RAM + battery (+ rumble)
    Case {
        cart_type: 0x1B,
        ram_code: 0x02,
        banks: 1,
        rtc: false,
    },
    Case {
        cart_type: 0x1B,
        ram_code: 0x03,
        banks: 4,
        rtc: false,
    },
    Case {
        cart_type: 0x1B,
        ram_code: 0x05,
        banks: 8,
        rtc: false,
    },
    Case {
        cart_type: 0x1B,
        ram_code: 0x04,
        banks: 16,
        rtc: false,
    },
    Case {
        cart_type: 0x1E,
        ram_code: 0x03,
        banks: 4,
        rtc: false,
    },
    // ROM only + RAM + battery
    Case {
        cart_type: 0x09,
        ram_code: 0x02,
        banks: 1,
        rtc: false,
    },
];

impl Case {
    fn ram_len(&self) -> usize {
        self.banks * RAM_BANK
    }

    fn save_len(&self) -> usize {
        self.ram_len() + if self.rtc { RTC_FOOTER } else { 0 }
    }

    /// Does the cart type have a bank register at 0x4000?
    fn banked(&self) -> bool {
        self.cart_type != 0x09
    }
}

/// Program that enables RAM and stores random bytes into every bank. Returns
/// the ROM and the expected RAM image.
fn writer_rom(case: Case, rng: &mut Rng) -> (Vec<u8>, Vec<u8>) {
    let mut model = vec![0u8; case.ram_len()];
    let mut p = Program::default();
    p.write(0x0000, 0x0A); // RAM enable
    if matches!(case.cart_type, 0x01..=0x03) {
        p.write(0x6000, 0x01); // MBC1 advanced banking mode: RAM banks reachable
    }
    for bank in 0..case.banks {
        if case.banked() {
            p.write(0x4000, bank as u8);
        }
        for _ in 0..120 {
            let off = rng.below(RAM_BANK);
            let v = rng.u8();
            p.write(0xA000 + off as u16, v);
            model[bank * RAM_BANK + off] = v;
        }
        // Make sure the first and last byte of each bank are covered.
        for off in [0, RAM_BANK - 1] {
            let v = rng.u8() | 1;
            p.write(0xA000 + off as u16, v);
            model[bank * RAM_BANK + off] = v;
        }
    }
    p.spin();
    // 1 MiB ROM keeps the MBC1 high-bank bits harmless for code in bank 0:
    // MBC1 advanced mode only remaps bank 0 for ROMs >= 1 MiB.
    let rom = make_rom(0x2_0000, case.cart_type, 2, case.ram_code, &p.0);
    (rom, model)
}

/// Program that only enables RAM and selects `bank`, so `peek` can read it.
fn reader_rom(case: Case, bank: usize) -> Vec<u8> {
    let mut p = Program::default();
    p.write(0x0000, 0x0A);
    if matches!(case.cart_type, 0x01..=0x03) {
        p.write(0x6000, 0x01);
    }
    if case.banked() {
        p.write(0x4000, bank as u8);
    }
    p.spin();
    make_rom(0x2_0000, case.cart_type, 2, case.ram_code, &p.0)
}

fn run(gb: &mut GameBoy, frames: usize) {
    for _ in 0..frames {
        gb.step_frame();
    }
}

#[test]
fn writes_survive_save_load_save() {
    for (i, case) in CASES.into_iter().enumerate() {
        let mut rng = Rng::new(0xBA77 + i as u64);
        let (rom, model) = writer_rom(case, &mut rng);

        let mut a = GameBoy::new(rom.clone()).unwrap();
        run(&mut a, 3);
        let save1 = a
            .battery_ram()
            .unwrap_or_else(|| panic!("{case:?}: no battery"));
        assert_eq!(save1.len(), case.save_len(), "{case:?}: save length");
        assert_eq!(
            save1[..case.ram_len()],
            model[..],
            "{case:?}: ROM writes missing from the save"
        );

        // Fresh machine (different program: it never writes the RAM itself).
        let mut b = GameBoy::new(reader_rom(case, 0)).unwrap();
        assert_ne!(b.battery_ram().unwrap()[..case.ram_len()], model[..]);
        b.load_battery_ram(&save1)
            .unwrap_or_else(|e| panic!("{case:?}: load failed: {e}"));
        assert_eq!(
            b.battery_ram().unwrap(),
            save1,
            "{case:?}: immediate re-save"
        );

        // The ROM sees the restored bytes through the cartridge bus.
        for bank in 0..case.banks {
            let mut r = GameBoy::new(reader_rom(case, bank)).unwrap();
            r.load_battery_ram(&save1).unwrap();
            run(&mut r, 1);
            for off in (0..RAM_BANK).step_by(61).chain([RAM_BANK - 1]) {
                assert_eq!(
                    r.peek(0xA000 + off as u16),
                    model[bank * RAM_BANK + off],
                    "{case:?}: bank {bank} offset {off:#x} after reload"
                );
            }
        }

        // Second generation is stable too.
        let save2 = b.battery_ram().unwrap();
        let mut c = GameBoy::new(reader_rom(case, 0)).unwrap();
        c.load_battery_ram(&save2).unwrap();
        assert_eq!(c.battery_ram().unwrap(), save1, "{case:?}: generation 3");
    }
}

/// Saves taken from a machine after the CPU keeps running (RTC ticking) still
/// round-trip exactly.
#[test]
fn rtc_save_round_trips_while_clock_runs() {
    let case = CASES[4];
    let mut rng = Rng::new(7);
    let (rom, _) = writer_rom(case, &mut rng);
    let mut a = GameBoy::new(rom).unwrap();
    run(&mut a, 140); // > 2 emulated seconds
    let save = a.battery_ram().unwrap();
    assert_eq!(save.len(), case.save_len());
    let mut b = GameBoy::new(reader_rom(case, 0)).unwrap();
    b.load_battery_ram(&save).unwrap();
    assert_eq!(b.battery_ram().unwrap(), save);
}

#[test]
fn malformed_save_sizes_never_panic() {
    for (i, case) in CASES.into_iter().enumerate() {
        let mut rng = Rng::new(0x5A7E + i as u64);
        let (rom, _) = writer_rom(case, &mut rng);
        let ram = case.ram_len();
        let mut gb = GameBoy::new(rom).unwrap();
        run(&mut gb, 2);
        let good = gb.battery_ram().unwrap();

        let lens = [
            0usize,
            1,
            2,
            ram.saturating_sub(1),
            ram + 1,
            ram + 4,
            ram + 43,
            ram + 44,
            ram + 45,
            ram + 47,
            ram + 48,
            ram + 49,
            ram + 96,
            2 * ram,
            RAM_BANK - 1,
            RAM_BANK + 1,
            1 << 20,
        ];
        for len in lens {
            let mut data = vec![0u8; len];
            rng.fill(&mut data);
            match gb.load_battery_ram(&data) {
                Ok(()) => {
                    // Graceful acceptance (e.g. the documented RTC footer
                    // sizes): the save keeps its shape and the machine runs.
                    assert_eq!(
                        gb.battery_ram().unwrap().len(),
                        good.len(),
                        "{case:?} len {len}"
                    );
                    run(&mut gb, 1);
                }
                Err(CartridgeError::SaveSizeMismatch { expected, actual }) => {
                    assert_eq!(actual, len);
                    assert_eq!(expected, ram);
                    // A rejected load must not have touched the RAM.
                    let before = gb.battery_ram().unwrap();
                    assert!(gb.load_battery_ram(&data).is_err());
                    assert_eq!(gb.battery_ram().unwrap(), before);
                }
                Err(e) => panic!("{case:?} len {len}: unexpected error {e}"),
            }
        }
        // Whatever happened above, a good save still loads.
        gb.load_battery_ram(&good).unwrap();
        assert_eq!(gb.battery_ram().unwrap(), good);
    }
}

/// Rejected loads leave the previous contents intact.
#[test]
fn rejected_load_preserves_ram() {
    let case = CASES[1];
    let mut rng = Rng::new(99);
    let (rom, model) = writer_rom(case, &mut rng);
    let mut gb = GameBoy::new(rom).unwrap();
    run(&mut gb, 2);
    for len in [0usize, 1, case.ram_len() - 1, case.ram_len() + 1] {
        assert!(gb.load_battery_ram(&vec![0xEE; len]).is_err(), "len {len}");
    }
    assert_eq!(gb.battery_ram().unwrap(), model);
}

/// Carts without a battery refuse saves and offer none.
#[test]
fn battery_less_carts_have_no_save() {
    for cart_type in [0x00u8, 0x01, 0x02, 0x08, 0x11, 0x12, 0x19, 0x1A, 0x1D] {
        let mut gb = GameBoy::new(make_rom(0x8000, cart_type, 0, 0x03, &[0x18, 0xFE])).unwrap();
        gb.step_frame();
        assert_eq!(gb.battery_ram(), None, "type {cart_type:#04x}");
        for len in [0usize, 1, 0x2000, 0x8000] {
            assert_eq!(
                gb.load_battery_ram(&vec![0; len]),
                Err(CartridgeError::NoBattery),
                "type {cart_type:#04x} len {len}"
            );
        }
    }
}

/// Battery carts with no RAM (or only a timer) must not panic on any save.
#[test]
fn battery_without_ram_is_safe() {
    for cart_type in [0x03u8, 0x13, 0x1B, 0x1E, 0x0F, 0x10, 0x09] {
        let mut gb = GameBoy::new(make_rom(0x8000, cart_type, 0, 0x00, &[0x18, 0xFE])).unwrap();
        gb.step_frame();
        let save = gb.battery_ram();
        if let Some(s) = &save {
            gb.load_battery_ram(s).unwrap();
        }
        for len in [0usize, 1, 44, 48, 0x2000] {
            let _ = gb.load_battery_ram(&vec![0xFF; len]);
        }
        gb.step_frame();
    }
}

/// Hostile RTC footers (out-of-range fields, all-ones) load without a panic
/// and the clock keeps running afterwards, including day-counter overflow.
#[test]
fn hostile_rtc_footers_never_panic() {
    let case = CASES[4];
    let mut rng = Rng::new(0x47C);
    for n in 0..14 {
        let (rom, _) = writer_rom(case, &mut rng);
        let mut gb = GameBoy::new(rom).unwrap();
        let mut save = vec![0u8; case.ram_len()];
        let mut footer = vec![0u8; RTC_FOOTER];
        match n {
            0 => footer.fill(0xFF),
            1 => footer.fill(0x00),
            _ => rng.fill(&mut footer),
        }
        if n >= 2 && n % 2 == 0 {
            // Wrap-around edge: 23:59:59 on day 511, running (not halted).
            for (w, v) in [59u8, 59, 23, 0xFF, 0x01].into_iter().enumerate() {
                footer[w * 4..w * 4 + 4].copy_from_slice(&u32::from(v).to_le_bytes());
            }
        }
        save.extend_from_slice(&footer);
        if n % 3 == 0 {
            save.truncate(save.len() - 4); // 44-byte short footer variant
        }
        gb.load_battery_ram(&save).unwrap();
        run(&mut gb, 125); // > 2 emulated seconds: forces ticks and rollover
        let out = gb.battery_ram().unwrap();
        assert_eq!(out.len(), case.save_len());
    }
}
