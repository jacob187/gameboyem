//! P3-CART save handling: the `--save-type` override, an EEPROM driven by CPU
//! stores, and the battery image with its RTC footer.

mod sys_common;
use emu_common::Core;
use gba_core::bus::CpuBus;
use gba_core::cart::{CartOptions, RtcMode, RtcTime, SaveChoice, SaveKind};
use gba_core::{CartError, Gba};
use sys_common::*;

const FOOTER: usize = gba_core::cart::RTC_FOOTER_LEN;
const EEPROM: u32 = 0x0D00_0000;
const SAVE: u32 = 0x0E00_0000;

fn gba_with(id: &[u8], save: SaveChoice, rtc: RtcMode) -> Gba {
    Gba::with_options(rom_with(id), None, CartOptions { save, rtc }).unwrap()
}

fn time(y: u16, mo: u8, d: u8, h: u8, mi: u8, s: u8) -> RtcTime {
    RtcTime::new(y, mo, d, h, mi, s).unwrap()
}

#[test]
fn the_override_picks_every_chip_whatever_the_rom_says() {
    // (ROM ID string, choice, detected kind, battery image length at power-on)
    let cases: [(&[u8], SaveChoice, SaveKind, Option<usize>); 14] = [
        (b"", SaveChoice::Auto, SaveKind::None, Option::None),
        (b"SRAM_V113", SaveChoice::Auto, SaveKind::Sram, Some(32768)),
        (
            b"FLASH1M_V103",
            SaveChoice::Auto,
            SaveKind::Flash128,
            Some(131072),
        ),
        (
            b"EEPROM_V124",
            SaveChoice::Auto,
            SaveKind::Eeprom,
            Option::None,
        ),
        (b"", SaveChoice::Sram, SaveKind::Sram, Some(32768)),
        (b"", SaveChoice::Flash64, SaveKind::Flash64, Some(65536)),
        (b"", SaveChoice::Flash128, SaveKind::Flash128, Some(131072)),
        (b"", SaveChoice::Eeprom512, SaveKind::Eeprom, Some(512)),
        (b"", SaveChoice::Eeprom8k, SaveKind::Eeprom, Some(8192)),
        (b"SRAM_V113", SaveChoice::None, SaveKind::None, Option::None),
        (
            b"FLASH1M_V103",
            SaveChoice::Flash64,
            SaveKind::Flash64,
            Some(65536),
        ),
        (
            b"EEPROM_V124",
            SaveChoice::Sram,
            SaveKind::Sram,
            Some(32768),
        ),
        (
            b"EEPROM_V124",
            SaveChoice::Eeprom8k,
            SaveKind::Eeprom,
            Some(8192),
        ),
        (
            b"FLASH_V126",
            SaveChoice::Flash128,
            SaveKind::Flash128,
            Some(131072),
        ),
    ];
    for (id, choice, kind, len) in cases {
        let g = gba_with(id, choice, RtcMode::Off);
        assert_eq!(g.save_kind(), kind, "{id:?} {choice:?}");
        assert_eq!(g.battery_ram().map(<[u8]>::len), len, "{id:?} {choice:?}");
        assert_eq!(Core::has_battery(&g), kind != SaveKind::None);
    }
}

fn bus_opts(id: &[u8], save: SaveChoice) -> gba_core::bus::Bus {
    let cart = gba_core::cart::Cart::with_options(
        rom_with(id),
        CartOptions {
            save,
            rtc: RtcMode::Off,
        },
    )
    .unwrap();
    gba_core::bus::Bus::new(cart, gba_core::bios::Bios::Hle)
}

#[test]
fn an_overridden_chip_behaves_like_that_chip_on_the_bus() {
    // Flash ID mode reports the 64K part, not the 128K one the ROM names.
    let mut b = bus_opts(b"FLASH1M_V103", SaveChoice::Flash64);
    for (a, v) in [(0x5555, 0xAA), (0x2AAA, 0x55), (0x5555, 0x90)] {
        b.write8(SAVE + a, v, N);
    }
    assert_eq!((b.read8(SAVE, N), b.read8(SAVE + 1, N)), (0x32, 0x1B));

    // SRAM forced on a ROM that names nothing mirrors every 32 KiB.
    let mut s = bus_opts(&[], SaveChoice::Sram);
    s.write8(SAVE + 9, 0x5A, N);
    assert_eq!(s.read8(SAVE + 9 + 0x8000, N), 0x5A);

    // No chip forced on an SRAM ROM: the window reads 0xFF and ignores stores.
    let mut n = bus_opts(b"SRAM_V113", SaveChoice::None);
    n.write8(SAVE, 1, N);
    assert_eq!(n.read8(SAVE, N), 0xFF);

    // A forced 512 B EEPROM answers on the bus from the first access.
    let mut e = bus_opts(&[], SaveChoice::Eeprom512);
    assert_eq!(e.read16(EEPROM, N), 1);
    cpu_send(&mut e, &request(false, 5, 6, 77));
    assert_eq!(e.read16(EEPROM, N), 1);
    cpu_send(&mut e, &request(true, 5, 6, 0));
    assert_eq!(cpu_read_value(&mut e), 77);
}

#[test]
fn a_forced_eeprom_size_refuses_a_save_of_the_other_size() {
    let mut e512 = gba_with(b"", SaveChoice::Eeprom512, RtcMode::Off);
    assert!(matches!(
        e512.load_battery_ram(&[0; 8192]),
        Err(CartError::SaveSize { got: 8192, .. })
    ));
    e512.load_battery_ram(&[7; 512]).unwrap();
    assert_eq!(e512.battery_ram().unwrap().len(), 512);

    let mut e8k = gba_with(b"", SaveChoice::Eeprom8k, RtcMode::Off);
    assert!(e8k.load_battery_ram(&[0; 512]).is_err());
    e8k.load_battery_ram(&[7; 8192]).unwrap();

    // Detected from the ROM, either size is fine and sets the size.
    let mut auto = gba_with(b"EEPROM_V124", SaveChoice::Auto, RtcMode::Off);
    auto.load_battery_ram(&[7; 512]).unwrap();
    assert_eq!(auto.battery_ram().unwrap().len(), 512);
}

/// One EEPROM request as 16-bit CPU stores to the chip's window, no DMA.
fn cpu_send(b: &mut gba_core::bus::Bus, bits: &[u8]) {
    for &bit in bits {
        b.write16(EEPROM, u16::from(bit), N);
    }
}

fn request(read: bool, addr: u32, addr_bits: u32, value: u64) -> Vec<u8> {
    let mut v = vec![1, u8::from(read)];
    v.extend((0..addr_bits).rev().map(|i| (addr >> i & 1) as u8));
    if !read {
        v.extend((0..64).rev().map(|i| (value >> i & 1) as u8));
    }
    v.push(0);
    v
}

fn cpu_read_value(b: &mut gba_core::bus::Bus) -> u64 {
    for _ in 0..4 {
        assert_eq!(b.read16(EEPROM, N), 0);
    }
    (0..64).fold(0, |v, _| v << 1 | u64::from(b.read16(EEPROM, N)))
}

#[test]
fn a_cpu_driven_eeprom_is_sized_from_the_request_length() {
    // 512 B: a 73-bit write, then the ready poll, then a 9-bit read.
    let mut b = bus_with(rom_with(b"EEPROM_V124"));
    assert!(b.cart.battery_ram().is_none());
    cpu_send(&mut b, &request(false, 0x15, 6, 0xDEAD_BEEF_0123_4567));
    assert!(
        b.cart.battery_ram().is_none(),
        "nothing has asked for a size yet"
    );
    assert_eq!(b.read16(EEPROM, N), 1, "the poll sees ready");
    assert_eq!(b.cart.battery_ram().unwrap().len(), 512);
    assert_eq!(b.cart.battery_generation(), 1, "the write was kept");
    cpu_send(&mut b, &request(true, 0x15, 6, 0));
    assert_eq!(cpu_read_value(&mut b), 0xDEAD_BEEF_0123_4567);

    // 8 KiB: an 81-bit write completes by itself.
    let mut b = bus_with(rom_with(b"EEPROM_V124"));
    cpu_send(&mut b, &request(false, 0x2AA, 14, 0x0102_0304_0506_0708));
    assert_eq!(b.cart.battery_ram().unwrap().len(), 8192);
    assert_eq!(b.read16(EEPROM, N), 1);
    cpu_send(&mut b, &request(true, 0x2AA, 14, 0));
    assert_eq!(cpu_read_value(&mut b), 0x0102_0304_0506_0708);

    // A read request alone: 17 bits is 8 KiB, 9 bits at the first read is 512 B.
    let mut b = bus_with(rom_with(b"EEPROM_V124"));
    cpu_send(&mut b, &request(true, 3, 6, 0));
    assert_eq!(b.read16(EEPROM, N), 0, "a read answers once it is sized");
    assert_eq!(b.cart.battery_ram().unwrap().len(), 512);
    let mut b = bus_with(rom_with(b"EEPROM_V124"));
    cpu_send(&mut b, &request(true, 3, 14, 0));
    assert_eq!(b.cart.battery_ram().unwrap().len(), 8192);

    // A first poll with no request pending decides nothing.
    let mut b = bus_with(rom_with(b"EEPROM_V124"));
    assert_eq!(b.read16(EEPROM, N), 1);
    assert!(b.cart.battery_ram().is_none());
}

#[test]
fn the_battery_image_has_a_footer_only_when_the_clock_is_in_use() {
    // Auto, never touched: the image is the bare chip, as before P3-CART.
    let g = gba_with(b"SRAM_V113", SaveChoice::Auto, RtcMode::Auto);
    assert_eq!(g.battery_save().unwrap().len(), 32768);
    assert_eq!(Core::battery_ram(&g).unwrap().len(), 32768);
    // On: the footer follows the data.
    let g = gba_with(b"SRAM_V113", SaveChoice::Auto, RtcMode::On);
    let img = g.battery_save().unwrap();
    assert_eq!(img.len(), 32768 + FOOTER);
    assert_eq!(&img[32768..32768 + 8], b"AGBRTC01");
    assert_eq!(
        Core::battery_ram(&g).unwrap(),
        img,
        "the Core view is the image"
    );
    assert_eq!(g.battery_ram().unwrap().len(), 32768, "the raw chip is not");
    // Off with nothing loaded: none. No chip: no image at all.
    let g = gba_with(b"SRAM_V113", SaveChoice::Auto, RtcMode::Off);
    assert_eq!(g.battery_save().unwrap().len(), 32768);
    let g = gba_with(b"", SaveChoice::Auto, RtcMode::On);
    assert!(g.battery_save().is_none());
    // An EEPROM of unknown size has no image yet, footer or not.
    let g = gba_with(b"EEPROM_V124", SaveChoice::Auto, RtcMode::On);
    assert!(g.battery_save().is_none());
}

#[test]
fn the_footer_round_trips_the_clock_through_a_new_machine() {
    let mut a = gba_with(b"FLASH1M_V103", SaveChoice::Auto, RtcMode::On);
    a.set_rtc(time(2024, 12, 31, 23, 59, 58));
    for _ in 0..30 {
        a.step_frame(); // about half a second
    }
    let img = a.battery_save().unwrap();
    assert_eq!(img.len(), 131072 + FOOTER);

    let mut b = gba_with(b"FLASH1M_V103", SaveChoice::Auto, RtcMode::On);
    b.load_battery_ram(&img).unwrap();
    assert_eq!(b.rtc_time(), a.rtc_time());
    assert_eq!(b.rtc_time(), time(2024, 12, 31, 23, 59, 58));
    // The sub-second remainder came along: the image is reproduced exactly.
    assert_eq!(b.battery_save().unwrap(), img);
    // About 1.5 s more on each machine crosses the year end together.
    for _ in 0..95 {
        a.step_frame();
        b.step_frame();
    }
    assert_eq!(b.rtc_time(), a.rtc_time());
    assert_eq!(a.rtc_time(), time(2025, 1, 1, 0, 0, 0));
    assert_eq!(b.battery_ram().unwrap(), a.battery_ram().unwrap());
}

#[test]
fn a_footer_loads_even_where_the_clock_is_not_yet_in_use() {
    let mut a = gba_with(b"SRAM_V113", SaveChoice::Auto, RtcMode::On);
    a.set_rtc(time(2010, 5, 5, 5, 5, 5));
    let img = a.battery_save().unwrap();
    // Auto, the game has not touched the port: the saved clock is kept and
    // written back unchanged in form (still a valid footer).
    let mut b = gba_with(b"SRAM_V113", SaveChoice::Auto, RtcMode::Auto);
    b.load_battery_ram(&img).unwrap();
    let again = b.battery_save().unwrap();
    assert_eq!(again.len(), img.len());
    assert_eq!(b.rtc_time(), time(2010, 5, 5, 5, 5, 5));
    // --rtc off carries it across frozen: time does not pass.
    let mut off = gba_with(b"SRAM_V113", SaveChoice::Auto, RtcMode::Off);
    off.load_battery_ram(&img).unwrap();
    for _ in 0..70 {
        off.step_frame();
    }
    assert_eq!(off.battery_save().unwrap(), img);
}

#[test]
fn saves_without_a_footer_still_load() {
    let mut g = gba_with(b"FLASH_V126", SaveChoice::Auto, RtcMode::On);
    g.set_rtc(time(2024, 6, 1, 12, 0, 0));
    let bare = vec![0x5A; 65536];
    g.load_battery_ram(&bare).unwrap();
    assert_eq!(g.battery_ram().unwrap(), &bare[..]);
    assert_eq!(
        g.rtc_time(),
        time(2024, 6, 1, 12, 0, 0),
        "the clock is left alone"
    );
    assert_eq!(g.battery_save().unwrap().len(), 65536 + FOOTER);
    // Every chip accepts its bare size and its size plus a footer.
    let footer = g.battery_save().unwrap()[65536..].to_vec();
    for (id, size) in [
        (&b"SRAM_V113"[..], 32768),
        (b"FLASH_V126", 65536),
        (b"FLASH1M_V103", 131072),
        (b"EEPROM_V124", 512),
        (b"EEPROM_V124", 8192),
    ] {
        let mut g = gba_with(id, SaveChoice::Auto, RtcMode::Auto);
        g.load_battery_ram(&vec![1; size]).unwrap();
        let mut g = gba_with(id, SaveChoice::Auto, RtcMode::Auto);
        let mut with = vec![1; size];
        with.extend_from_slice(&footer);
        g.load_battery_ram(&with).unwrap();
        assert_eq!(g.battery_ram().unwrap().len(), size, "{id:?} {size}");
    }
}

#[test]
fn a_corrupt_footer_or_size_is_rejected_and_changes_nothing() {
    let mut a = gba_with(b"SRAM_V113", SaveChoice::Auto, RtcMode::On);
    a.set_rtc(time(2024, 2, 29, 1, 2, 3));
    let good = a.battery_save().unwrap();

    let mut g = gba_with(b"SRAM_V113", SaveChoice::Auto, RtcMode::On);
    g.set_rtc(time(2001, 1, 1, 0, 0, 0));
    let before = (g.battery_save(), g.battery_generation(), g.rtc_time());
    let n = 32768;
    let mutations: Vec<(&str, Vec<u8>)> = {
        let flip = |i: usize, v: u8| {
            let mut m = good.clone();
            m[n + i] = v;
            m
        };
        vec![
            ("magic", flip(0, b'X')),
            ("control", flip(8, 0x01)),
            ("bcd", flip(9, 0xFA)),
            ("month", flip(10, 0x13)),
            ("feb 30", flip(11, 0x30)),
            ("weekday", flip(12, 9)),
            ("hour", flip(13, 0x24)),
            ("subsecond", flip(19, 0xFF)),
            ("reserved", flip(21, 1)),
            ("truncated", good[..good.len() - 1].to_vec()),
            ("extra byte", [good.clone(), vec![0]].concat()),
            ("only a footer", good[n..].to_vec()),
            ("empty", vec![]),
        ]
    };
    for (what, img) in mutations {
        let err = g.load_battery_ram(&img).unwrap_err();
        assert!(
            matches!(err, CartError::BadRtcFooter(_) | CartError::SaveSize { .. }),
            "{what}: {err:?}"
        );
        assert_eq!(
            (g.battery_save(), g.battery_generation(), g.rtc_time()),
            before,
            "{what}: a rejected file changes nothing"
        );
    }
    // The real footer is fine.
    g.load_battery_ram(&good).unwrap();
    assert_eq!(g.rtc_time(), time(2024, 2, 29, 1, 2, 3));
    // A footer for a cartridge with no chip is refused outright.
    let mut none = gba_with(b"", SaveChoice::Auto, RtcMode::On);
    assert_eq!(none.load_battery_ram(&good), Err(CartError::NoSave));
}

#[test]
fn the_game_writing_the_clock_marks_the_save_changed_but_time_passing_does_not() {
    let mut g = gba_with(b"SRAM_V113", SaveChoice::Auto, RtcMode::On);
    g.set_rtc(time(2024, 1, 1, 0, 0, 0));
    let generation = g.battery_generation();
    let img = g.battery_save().unwrap();
    for _ in 0..70 {
        g.step_frame();
    }
    assert_eq!(
        g.battery_generation(),
        generation,
        "like gb-core's free-running RTC"
    );
    assert_ne!(g.battery_save().unwrap(), img, "but the image moved on");
    assert_eq!(g.rtc_time(), time(2024, 1, 1, 0, 0, 1));
}

#[test]
fn set_rtc_clears_the_power_loss_flag_and_rejects_nothing_valid() {
    let mut g = gba_with(b"SRAM_V113", SaveChoice::Auto, RtcMode::On);
    let fresh = g.battery_save().unwrap();
    assert_eq!(fresh[32768 + 8], 0x82, "a fresh clock says power was lost");
    g.set_rtc(time(2000, 1, 1, 0, 0, 0));
    assert_eq!(g.battery_save().unwrap()[32768 + 8], 0x02);
    assert_eq!(
        RtcTime::new(2100, 1, 1, 0, 0, 0),
        Err(CartError::BadRtcTime)
    );
    assert_eq!(
        RtcTime::new(2023, 2, 29, 0, 0, 0),
        Err(CartError::BadRtcTime)
    );
}

#[test]
fn a_host_set_clock_is_saved_even_before_the_game_uses_the_port() {
    let mut g = gba_with(b"SRAM_V113", SaveChoice::Auto, RtcMode::Auto);
    assert_eq!(g.battery_save().unwrap().len(), 32768);
    let generation = g.battery_generation();
    g.set_rtc(time(2024, 7, 4, 9, 30, 0));
    assert_ne!(g.battery_generation(), generation);
    let img = g.battery_save().unwrap();
    assert_eq!(img.len(), 32768 + FOOTER);
    let mut b = gba_with(b"SRAM_V113", SaveChoice::Auto, RtcMode::Auto);
    b.load_battery_ram(&img).unwrap();
    assert_eq!(b.rtc_time(), time(2024, 7, 4, 9, 30, 0));
}
