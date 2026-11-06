//! The cartridge GPIO port and its S-3511 clock, bit-banged through the bus the
//! way a game's driver does it: 16-bit stores to 0x0800_00C4/C6/C8, command byte
//! MSB first, data bytes LSB first, bits sampled when SCK rises.

mod cart_common;
mod sys_common;
use cart_common::*;
use gba_core::bus::CpuBus;
use gba_core::cart::RtcMode;
use sys_common::*;

#[test]
fn registers_read_as_rom_until_the_game_enables_reads() {
    let mut b = bus_in(RtcMode::On);
    assert_eq!(b.read16(DATA, N), 0x2211);
    assert_eq!(b.read16(DIR, N), 0x4433);
    assert_eq!(b.read32(DATA, N), 0x4433_2211);
    assert_eq!(b.read8(DATA + 1, N), 0x22);
    // Writes land in the latches, but the addresses still read the ROM.
    b.write16(DIR, 7, N);
    b.write16(DATA, 5, N);
    assert_eq!(b.read16(DATA, N), 0x2211);
    // Read-enable set: the registers appear, narrower than the ROM halfwords.
    b.write16(CTRL, 1, N);
    assert_eq!(b.read16(DATA, N), 5);
    assert_eq!(b.read16(DIR, N), 7);
    assert_eq!(b.read16(CTRL, N), 1);
    assert_eq!(
        b.read32(DATA, N),
        5 | 7 << 16,
        "data and direction as one word"
    );
    assert_eq!(b.read8(DIR, N), 7);
    assert_eq!(b.read8(DIR + 1, N), 0);
    assert_eq!(b.read32(CTRL, N), 1, "control, then the ROM beyond it");
    // Past the three registers it is plain ROM, and peeks agree.
    assert_eq!(b.peek16(DATA + 6), 0x0000);
    assert_eq!(b.peek16(DATA), 5);
    // Reads off again: ROM.
    b.write16(CTRL, 0, N);
    assert_eq!(b.read16(DATA, N), 0x2211);
}

#[test]
fn direction_masks_which_pins_the_gba_drives_and_stores_four_bits() {
    let mut b = bus_in(RtcMode::On);
    b.write16(CTRL, 1, N);
    b.write16(DIR, 0b0011, N);
    b.write16(DATA, 0b1111, N);
    assert_eq!(
        b.read16(DATA, N),
        0b0011,
        "input pins do not come from the latch"
    );
    b.write16(DIR, 0xFFF4, N);
    assert_eq!(b.read16(DIR, N), 4);
    b.write16(DATA, 0xFFFF, N);
    assert_eq!(b.read16(DATA, N), 4);
    // A word store at 0xC4 sets data then direction.
    b.write32(DATA, 0x0003_000F, N);
    assert_eq!((b.read16(DATA, N), b.read16(DIR, N)), (3, 3));
    // A byte store is ignored (GBATEK).
    b.write8(DIR, 0, N);
    assert_eq!(b.read16(DIR, N), 3);
    // Control keeps only bit 0.
    b.write16(CTRL, 0xFFFF, N);
    assert_eq!(b.read16(CTRL, N), 1);
}

#[test]
fn a_fresh_clock_reports_power_loss_once_and_runs_the_reset_sequence() {
    let mut b = bus_in(RtcMode::On);
    init(&mut b);
    assert_eq!(read_reg(&mut b, 1, 1), [0x82], "power flag set until read");
    assert_eq!(read_reg(&mut b, 1, 1), [0x02], "the read cleared it");
    // What a game does when it sees the flag: force reset, then 24h mode.
    reset(&mut b);
    assert_eq!(read_reg(&mut b, 1, 1), [0x00]);
    assert_eq!(read_reg(&mut b, 2, 7), [0x00, 0x01, 0x01, 0, 0, 0, 0]);
    write_reg(&mut b, 1, &[0x40]);
    assert_eq!(read_reg(&mut b, 1, 1), [0x40]);
    // Only the writable bits stick.
    write_reg(&mut b, 1, &[0xFF]);
    assert_eq!(read_reg(&mut b, 1, 1), [0x6A]);
}

#[test]
fn datetime_and_time_round_trip_in_bcd() {
    let mut b = bus_in(RtcMode::On);
    init(&mut b);
    write_reg(&mut b, 1, &[0x40]);
    write_reg(&mut b, 2, &[0x23, 0x11, 0x30, 4, 0x17, 0x42, 0x09]);
    assert_eq!(
        read_reg(&mut b, 2, 7),
        [0x23, 0x11, 0x30, 4, 0x17, 0x42, 0x09]
    );
    assert_eq!(read_reg(&mut b, 3, 3), [0x17, 0x42, 0x09]);
    write_reg(&mut b, 3, &[0x08, 0x15, 0x59]);
    assert_eq!(
        read_reg(&mut b, 2, 7),
        [0x23, 0x11, 0x30, 4, 0x08, 0x15, 0x59]
    );
    // An impossible date is ignored field by field: Nov 31 keeps day 30.
    write_reg(&mut b, 2, &[0x23, 0x11, 0x31, 4, 0x08, 0x15, 0x59]);
    assert_eq!(read_reg(&mut b, 2, 7)[2], 0x30);
}

#[test]
fn the_hour_register_follows_the_12_24_hour_bit() {
    let mut b = bus_in(RtcMode::On);
    init(&mut b);
    write_reg(&mut b, 1, &[0x40]);
    write_reg(&mut b, 3, &[0x15, 0x04, 0x05]);
    assert_eq!(read_reg(&mut b, 3, 3), [0x15, 0x04, 0x05]);
    // Same moment in 12-hour mode: 3 PM, flag in bit 7.
    write_reg(&mut b, 1, &[0x00]);
    assert_eq!(read_reg(&mut b, 3, 3), [0x83, 0x04, 0x05]);
    write_reg(&mut b, 3, &[0x07, 0x00, 0x00]);
    assert_eq!(read_reg(&mut b, 3, 3)[0], 0x07, "7 AM");
    write_reg(&mut b, 3, &[0x87, 0x00, 0x00]);
    assert_eq!(read_reg(&mut b, 3, 3)[0], 0x87, "7 PM");
    write_reg(&mut b, 1, &[0x40]);
    assert_eq!(read_reg(&mut b, 3, 3)[0], 0x19, "19:00 in 24-hour mode");
}

/// Set a time, let two emulated seconds pass, read it back.
fn two_seconds_across_a_leap_day() -> Vec<u8> {
    let mut b = bus_in(RtcMode::On);
    init(&mut b);
    write_reg(&mut b, 1, &[0x40]);
    // Leap day: 2024-02-28 23:59:58 plus two seconds is 02-29 00:00:00, a Thursday.
    write_reg(&mut b, 2, &[0x24, 0x02, 0x28, 3, 0x23, 0x59, 0x58]);
    // Reading costs only bus cycles, far under a second: nothing moves.
    assert_eq!(read_reg(&mut b, 3, 3), [0x23, 0x59, 0x58]);
    run(&mut b, CPS);
    assert_eq!(read_reg(&mut b, 3, 3), [0x23, 0x59, 0x59]);
    run(&mut b, CPS);
    read_reg(&mut b, 2, 7)
}

#[test]
fn time_advances_only_with_emulated_cycles_and_rolls_over() {
    assert_eq!(
        two_seconds_across_a_leap_day(),
        [0x24, 0x02, 0x29, 4, 0x00, 0x00, 0x00],
        "minutes, hours, day and weekday carried"
    );
    // No host clock anywhere: a second run is bit-identical.
    assert_eq!(
        two_seconds_across_a_leap_day(),
        two_seconds_across_a_leap_day()
    );
}

#[test]
fn force_irq_raises_the_game_pak_interrupt() {
    let mut b = bus_in(RtcMode::On);
    init(&mut b);
    assert_eq!(b.irq.if_ & 1 << 13, 0);
    transfer(&mut b, 6, false, |_| vec![]);
    assert_eq!(b.irq.if_ & 1 << 13, 1 << 13);
}

#[test]
fn a_transfer_cut_short_writes_nothing_and_the_next_one_is_clean() {
    let mut b = bus_in(RtcMode::On);
    init(&mut b);
    write_reg(&mut b, 1, &[0x40]);
    write_reg(&mut b, 3, &[0x01, 0x02, 0x03]);
    transfer(&mut b, 3, false, |b| {
        write_data(b, &[0x20, 0x30]);
        vec![]
    });
    assert_eq!(read_reg(&mut b, 3, 3), [0x01, 0x02, 0x03]);
}

#[test]
fn detection_auto_wakes_on_plausible_gpio_setup_only() {
    // Auto: a stray store changes nothing, then the first plausible one wakes it.
    let mut b = bus_in(RtcMode::Auto);
    b.write16(CTRL, 1, N);
    assert!(b.cart.gpio_live(), "enabling reads is GPIO setup");
    let mut b = bus_in(RtcMode::Auto);
    b.write16(DATA, 0xBEEF, N);
    b.write16(CTRL, 0x1234, N);
    b.write8(DATA, 1, N);
    b.write16(0x0800_0000, 0, N);
    assert!(!b.cart.gpio_live());
    assert_eq!(b.read16(DATA, N), 0x2211);
    init(&mut b);
    assert!(b.cart.gpio_live());
    assert_eq!(read_reg(&mut b, 1, 1), [0x82]);

    // On: live from power-on, before any write.
    assert!(bus_in(RtcMode::On).cart.gpio_live());

    // Off: the same setup does nothing and the addresses stay ROM.
    let mut b = bus_in(RtcMode::Off);
    init(&mut b);
    assert!(!b.cart.gpio_live());
    assert_eq!(b.read16(DATA, N), 0x2211);
    assert_eq!(b.read16(CTRL, N), 0x6655);
}

#[test]
fn a_cart_that_never_touches_the_port_is_untouched() {
    let mut b = bus_in(RtcMode::Auto);
    for a in (0xC0..0xD0).step_by(2) {
        assert_eq!(
            b.read16(0x0800_0000 + a, N),
            u16::from_le_bytes([b.cart.rom()[a as usize], b.cart.rom()[a as usize + 1]])
        );
    }
    assert!(b.cart.battery_save(0).is_none(), "no chip, no save");
}

/// The sequence of the standard Sii driver (pokeemerald `SiiRtcGetStatus`): read
/// enable, DATA = SCK, DATA = SCK|CS, and only then DIRECTION = all-out, which
/// raises CS and SCK in one update. That SCK edge is not a data clock.
#[test]
fn the_standard_drivers_first_transfer_works_from_a_fresh_auto_cart() {
    let mut b = bus_in(RtcMode::Auto);
    b.write16(CTRL, 1, N);
    b.write16(DATA, SCK, N);
    b.write16(DATA, SCK | CS, N);
    b.write16(DIR, 7, N);
    send_command(&mut b, 1, true);
    assert_eq!(
        read_data(&mut b, 1),
        [0x82],
        "power-loss flag on a fresh clock"
    );
}
