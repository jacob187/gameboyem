//! The Seiko S-3511 real-time clock behind the cartridge's GPIO port (GBATEK "GBA
//! Cart Real-Time Clock (RTC)"), as a register file plus calendar arithmetic.
//! The serial protocol that reaches it is in `gpio.rs`.
//!
//! **Time base (DECISIONS #66).** The clock holds a calendar date and time and a
//! sub-second remainder counted in master-clock cycles. It advances only when
//! told the current emulated cycle ([`Rtc::advance_to`]); no host clock is read
//! anywhere in the core. The host's only way in is [`RtcTime`] through
//! `Gba::set_rtc`.
//!
//! Registers are held in binary and encoded to BCD on read, as the chip presents
//! them: `year month day weekday hour minute second`, each BCD except the weekday
//! (0..=6, free-running, not derived from the date). In 12-hour mode (control
//! bit 6 clear) the hour is 0..=11 and bit 7 is the PM flag; in 24-hour mode it is
//! 0..=23 with bit 7 clear (a game that reads the hour as plain BCD is never
//! handed a stray flag).

use super::CartError;
use crate::CYCLES_PER_SECOND;
use crate::state::{StateError, check};
use emu_common::state::{StateReader, StateWriter};

/// Control register: bit 7 is the power-off flag (read-only; set when the clock
/// lost power, cleared by reading the register or by a reset).
pub const CTRL_POWER: u8 = 0x80;
/// Control register: set = 24-hour mode.
pub const CTRL_24H: u8 = 0x40;
/// Bits a game can write: 1 (IRQ duty/hold), 3 (per-minute IRQ), 5 (unknown), 6 (24h).
const CTRL_WRITABLE: u8 = 0x6A;
/// The control register after power was lost (GBATEK: "82h after a battery shortcut").
const CTRL_POWER_ON: u8 = 0x82;

/// Size of the persisted RTC footer (see [`Rtc::footer`]).
pub const FOOTER_LEN: usize = 24;
const FOOTER_MAGIC: [u8; 8] = *b"AGBRTC01";

const CPS: u64 = CYCLES_PER_SECOND as u64;

/// Days in the 100-year cycle of the two-digit year (see [`days_in_month`]).
const DAYS_PER_CENTURY: u64 = 36_525;

/// A calendar date and time, as the host supplies it to start the clock. The
/// RTC has a two-digit year, so only 2000..=2099 exists. The fields are private:
/// [`RtcTime::new`] is the only way to make one, so every value is a real date
/// (day valid for its month and year, time of day in range) with the weekday
/// computed from it (code-review K#4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtcTime {
    year: u16,
    month: u8,
    day: u8,
    weekday: u8,
    hour: u8,
    minute: u8,
    second: u8,
}

impl RtcTime {
    pub fn year(&self) -> u16 {
        self.year
    }

    pub fn month(&self) -> u8 {
        self.month
    }

    pub fn day(&self) -> u8 {
        self.day
    }

    /// 0 = Sunday.
    pub fn weekday(&self) -> u8 {
        self.weekday
    }

    pub fn hour(&self) -> u8 {
        self.hour
    }

    pub fn minute(&self) -> u8 {
        self.minute
    }

    pub fn second(&self) -> u8 {
        self.second
    }

    /// A validated time. The weekday follows from the date.
    pub fn new(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Result<Self, CartError> {
        let y = year.wrapping_sub(2000);
        let ok = y <= 99
            && (1..=12).contains(&month)
            && (1..=days_in_month(y as u8, month)).contains(&day)
            && hour <= 23
            && minute <= 59
            && second <= 59;
        if !ok {
            return Err(CartError::BadRtcTime);
        }
        Ok(Self {
            year,
            month,
            day,
            weekday: weekday_of(y as u8, month, day),
            hour,
            minute,
            second,
        })
    }
}

/// Days in `month` (1..=12) of the year `2000 + year`. A two-digit year is a leap
/// year when divisible by 4 (2000 is; 2100 is out of range).
pub(super) fn days_in_month(year: u8, month: u8) -> u8 {
    match month {
        2 if year % 4 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Weekday (0 = Sunday) of `2000 + year`-`month`-`day`. 2000-01-01 was a Saturday.
fn weekday_of(year: u8, month: u8, day: u8) -> u8 {
    let mut days = 0u32;
    for y in 0..year {
        days += if y % 4 == 0 { 366 } else { 365 };
    }
    for m in 1..month {
        days += u32::from(days_in_month(year, m));
    }
    days += u32::from(day) - 1;
    ((days + 6) % 7) as u8
}

fn to_bcd(v: u8) -> u8 {
    (v / 10) << 4 | (v % 10)
}

fn from_bcd(b: u8) -> Option<u8> {
    let (hi, lo) = (b >> 4, b & 15);
    (hi <= 9 && lo <= 9).then_some(hi * 10 + lo)
}

/// The S-3511's registers and its running clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Rtc {
    pub control: u8,
    year: u8,
    month: u8,
    day: u8,
    weekday: u8,
    hour: u8,
    minute: u8,
    second: u8,
    /// Master-clock cycles into the current second (< `CYCLES_PER_SECOND`).
    subsecond: u32,
    /// The master-clock cycle the fields above are valid for.
    synced_at: u64,
}

impl Rtc {
    /// The state of a clock that has just lost power: reset time, power flag set.
    pub fn power_on() -> Self {
        let mut r = Self {
            control: 0,
            year: 0,
            month: 1,
            day: 1,
            weekday: 0,
            hour: 0,
            minute: 0,
            second: 0,
            subsecond: 0,
            synced_at: 0,
        };
        r.reset();
        r.control = CTRL_POWER_ON;
        r
    }

    /// Every register, the sub-second count and the cycle they are valid for.
    /// The fields are binary here (BCD only exists on the wire), so the save
    /// stores them as they are.
    pub fn save_state(&self, w: &mut StateWriter) {
        w.u8(self.control);
        w.u8(self.year);
        w.u8(self.month);
        w.u8(self.day);
        w.u8(self.weekday);
        w.u8(self.hour);
        w.u8(self.minute);
        w.u8(self.second);
        w.u32(self.subsecond);
        w.u64(self.synced_at);
    }

    /// The clock never holds an impossible time (`write_datetime` and
    /// `write_time` ignore a field that is not valid BCD in range), so every
    /// field is range-checked, the day against its month and year. `synced_at`
    /// is checked against the restored clock by the cart ([`Rtc::synced_at`]).
    pub fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let next = Self {
            control: r.u8()?,
            year: r.u8()?,
            month: r.u8()?,
            day: r.u8()?,
            weekday: r.u8()?,
            hour: r.u8()?,
            minute: r.u8()?,
            second: r.u8()?,
            subsecond: r.u32()?,
            synced_at: r.u64()?,
        };
        check(
            next.control & !(CTRL_POWER | CTRL_WRITABLE) == 0,
            "RTC control",
        )?;
        check(next.year <= 99, "RTC year")?;
        check((1..=12).contains(&next.month), "RTC month")?;
        check(
            (1..=days_in_month(next.year, next.month)).contains(&next.day),
            "RTC day",
        )?;
        check(next.weekday <= 6, "RTC weekday")?;
        check(
            next.hour <= 23 && next.minute <= 59 && next.second <= 59,
            "RTC time of day",
        )?;
        check(u64::from(next.subsecond) < CPS, "RTC sub-second count")?;
        check(next.synced_at < crate::state::MAX_CLOCK, "RTC sync time")?;
        *self = next;
        Ok(())
    }

    /// The master-clock cycle the fields are valid for.
    pub fn synced_at(&self) -> u64 {
        self.synced_at
    }

    /// Force reset: every register to 0, except day and month which become 1.
    pub fn reset(&mut self) {
        *self = Self {
            control: 0,
            year: 0,
            month: 1,
            day: 1,
            weekday: 0,
            hour: 0,
            minute: 0,
            second: 0,
            subsecond: 0,
            synced_at: self.synced_at,
        };
    }

    /// Start the clock at the host-supplied time (the clock is known good, so the
    /// power flag clears; the 12/24h mode is kept).
    pub fn set_time(&mut self, t: RtcTime, now: u64) {
        self.year = (t.year - 2000) as u8;
        self.month = t.month;
        self.day = t.day;
        self.weekday = t.weekday;
        self.hour = t.hour;
        self.minute = t.minute;
        self.second = t.second;
        self.subsecond = 0;
        self.synced_at = now;
        self.control &= !CTRL_POWER;
    }

    pub fn time(&self) -> RtcTime {
        RtcTime {
            year: 2000 + u16::from(self.year),
            month: self.month,
            day: self.day,
            weekday: self.weekday,
            hour: self.hour,
            minute: self.minute,
            second: self.second,
        }
    }

    /// Catch the clock up to master-clock cycle `now` (never backwards).
    pub fn advance_to(&mut self, now: u64) {
        let elapsed = now.saturating_sub(self.synced_at);
        self.synced_at = self.synced_at.max(now);
        let total = u64::from(self.subsecond) + elapsed;
        self.subsecond = (total % CPS) as u32;
        self.add_seconds(total / CPS);
    }

    /// A copy advanced to `now`; `self` is untouched.
    pub fn at(&self, now: u64) -> Self {
        let mut r = self.clone();
        r.advance_to(now);
        r
    }

    fn add_seconds(&mut self, secs: u64) {
        if secs == 0 {
            return;
        }
        let of_day =
            u64::from(self.hour) * 3600 + u64::from(self.minute) * 60 + u64::from(self.second);
        let total = of_day + secs;
        let (days, rem) = (total / 86_400, total % 86_400);
        self.hour = (rem / 3600) as u8;
        self.minute = (rem % 3600 / 60) as u8;
        self.second = (rem % 60) as u8;
        // A calendar day at a time. The weekday cycle repeats every 7 days, and
        // the date every 100 years (the year wraps at 100 and every year divisible
        // by 4 is a leap year, so 25 * 366 + 75 * 365 = 36 525 days), so only the
        // date needs the loop, and only for the days left after whole periods
        // (deep-review #22).
        self.weekday = ((u64::from(self.weekday) + days) % 7) as u8;
        for _ in 0..days % DAYS_PER_CENTURY {
            self.next_day();
        }
    }

    fn next_day(&mut self) {
        if self.day < days_in_month(self.year, self.month) {
            self.day += 1;
            return;
        }
        self.day = 1;
        if self.month < 12 {
            self.month += 1;
        } else {
            self.month = 1;
            self.year = (self.year + 1) % 100;
        }
    }

    fn hour_byte(&self) -> u8 {
        if self.control & CTRL_24H != 0 {
            to_bcd(self.hour)
        } else {
            to_bcd(self.hour % 12) | if self.hour >= 12 { 0x80 } else { 0 }
        }
    }

    /// The 7 bytes the datetime register returns.
    pub fn read_datetime(&self) -> [u8; 7] {
        [
            to_bcd(self.year),
            to_bcd(self.month),
            to_bcd(self.day),
            self.weekday,
            self.hour_byte(),
            to_bcd(self.minute),
            to_bcd(self.second),
        ]
    }

    /// The 3 bytes the time register returns.
    pub fn read_time(&self) -> [u8; 3] {
        let d = self.read_datetime();
        [d[4], d[5], d[6]]
    }

    /// Decode an hour byte under the current 12/24h mode.
    fn parse_hour(&self, b: u8) -> Option<u8> {
        if self.control & CTRL_24H != 0 {
            from_bcd(b & 0x7F).filter(|&h| h <= 23)
        } else {
            let h = from_bcd(b & 0x7F)?;
            (h <= 11).then(|| h + if b & 0x80 != 0 { 12 } else { 0 })
        }
    }

    /// Write the time register. A field that is not valid BCD in range is
    /// ignored, so the clock never holds an impossible time. The divider
    /// restarts, as on the chip.
    pub fn write_time(&mut self, b: [u8; 3]) {
        if let Some(h) = self.parse_hour(b[0]) {
            self.hour = h;
        }
        if let Some(m) = from_bcd(b[1]).filter(|&m| m <= 59) {
            self.minute = m;
        }
        if let Some(s) = from_bcd(b[2]).filter(|&s| s <= 59) {
            self.second = s;
        }
        self.subsecond = 0;
    }

    /// Write the datetime register (same rules as [`Rtc::write_time`]).
    pub fn write_datetime(&mut self, b: [u8; 7]) {
        if let Some(y) = from_bcd(b[0]) {
            self.year = y;
        }
        if let Some(m) = from_bcd(b[1]).filter(|m| (1..=12).contains(m)) {
            self.month = m;
        }
        // The day is judged against the month and year just stored.
        if let Some(d) =
            from_bcd(b[2]).filter(|&d| (1..=days_in_month(self.year, self.month)).contains(&d))
        {
            self.day = d;
        }
        if b[3] <= 6 {
            self.weekday = b[3];
        }
        // A new month or year can leave the old day past its end (day 31, then
        // month 2; 29 February, then a non-leap year). The S-3511 is not
        // documented here; the clock never holds an impossible date, so the day
        // is clamped to the last day of the month it is now in.
        self.day = self.day.min(days_in_month(self.year, self.month));
        self.write_time([b[4], b[5], b[6]]);
    }

    /// Write the control register: only the writable bits change.
    pub fn write_control(&mut self, v: u8) {
        self.control = (self.control & CTRL_POWER) | (v & CTRL_WRITABLE);
    }

    /// The persisted form: 24 bytes, little-endian where multi-byte.
    ///
    /// ```text
    /// 0..8   "AGBRTC01"
    /// 8      control register (incl. the power flag)
    /// 9..16  year, month, day (BCD), weekday (0..=6), hour (BCD, 0..=23, no PM
    ///        flag), minute, second (BCD)
    /// 16..20 u32 cycles into the current second (< 16 777 216)
    /// 20..24 reserved, zero
    /// ```
    ///
    /// The fields are the clock's value at the cycle the save was taken; the
    /// sub-second remainder is the "emulated-time reference", so a save and a load
    /// lose nothing. No host timestamp is stored (DECISIONS #67).
    pub fn footer(&self) -> [u8; FOOTER_LEN] {
        let mut f = [0u8; FOOTER_LEN];
        f[..8].copy_from_slice(&FOOTER_MAGIC);
        f[8] = self.control;
        f[9] = to_bcd(self.year);
        f[10] = to_bcd(self.month);
        f[11] = to_bcd(self.day);
        f[12] = self.weekday;
        f[13] = to_bcd(self.hour);
        f[14] = to_bcd(self.minute);
        f[15] = to_bcd(self.second);
        f[16..20].copy_from_slice(&self.subsecond.to_le_bytes());
        f
    }

    /// Parse a footer from untrusted bytes: exact length, magic, every field in
    /// range and valid BCD, reserved bytes zero. The clock is taken as valid at
    /// cycle `now`.
    pub fn from_footer(f: &[u8], now: u64) -> Result<Self, CartError> {
        let bad = CartError::BadRtcFooter;
        if f.len() != FOOTER_LEN {
            return Err(bad("wrong length"));
        }
        if f[..8] != FOOTER_MAGIC {
            return Err(bad("bad magic"));
        }
        if f[8] & !(CTRL_POWER | CTRL_WRITABLE) != 0 {
            return Err(bad("control register has unused bits set"));
        }
        let field = |i: usize, lo: u8, hi: u8, what: &'static str| {
            from_bcd(f[i])
                .filter(|v| (lo..=hi).contains(v))
                .ok_or(bad(what))
        };
        let year = field(9, 0, 99, "year")?;
        let month = field(10, 1, 12, "month")?;
        let day = field(11, 1, days_in_month(year, month), "day")?;
        if f[12] > 6 {
            return Err(bad("weekday"));
        }
        let hour = field(13, 0, 23, "hour")?;
        let minute = field(14, 0, 59, "minute")?;
        let second = field(15, 0, 59, "second")?;
        let subsecond = u32::from_le_bytes([f[16], f[17], f[18], f[19]]);
        if u64::from(subsecond) >= CPS {
            return Err(bad("sub-second count"));
        }
        if f[20..].iter().any(|&b| b != 0) {
            return Err(bad("reserved bytes"));
        }
        Ok(Self {
            control: f[8],
            year,
            month,
            day,
            weekday: f[12],
            hour,
            minute,
            second,
            subsecond,
            synced_at: now,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: u16, mo: u8, d: u8, h: u8, mi: u8, s: u8) -> Rtc {
        let mut r = Rtc::power_on();
        r.set_time(RtcTime::new(y, mo, d, h, mi, s).unwrap(), 0);
        r.control = CTRL_24H;
        r
    }

    fn secs(n: u64) -> u64 {
        n * CPS
    }

    #[test]
    fn power_on_state_is_reset_with_the_power_flag() {
        let r = Rtc::power_on();
        assert_eq!(r.control, 0x82);
        assert_eq!(r.read_datetime(), [0, 1, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn seconds_roll_into_minutes_hours_and_days() {
        let mut r = at(2024, 3, 9, 23, 59, 58);
        r.advance_to(secs(1));
        assert_eq!(r.time().second, 59);
        r.advance_to(secs(2));
        let t = r.time();
        assert_eq!((t.day, t.hour, t.minute, t.second), (10, 0, 0, 0));
        // The weekday moves with the day: 2024-03-09 was a Saturday.
        assert_eq!(t.weekday, 0);
    }

    #[test]
    fn sub_second_remainder_is_exact_and_carries() {
        let mut r = at(2000, 1, 1, 0, 0, 0);
        r.advance_to(CPS - 1);
        assert_eq!(r.time().second, 0);
        r.advance_to(CPS);
        assert_eq!(r.time().second, 1);
        r.advance_to(CPS + CPS / 2);
        r.advance_to(2 * CPS + CPS / 2);
        assert_eq!(r.time().second, 2);
        assert_eq!(r.subsecond, (CPS / 2) as u32);
        // Time never runs backwards.
        r.advance_to(5);
        assert_eq!(r.time().second, 2);
    }

    #[test]
    fn month_lengths_and_leap_years() {
        // 2024 is a leap year: Feb 28 -> 29 -> Mar 1.
        let mut r = at(2024, 2, 28, 12, 0, 0);
        r.advance_to(secs(86_400));
        assert_eq!((r.time().month, r.time().day), (2, 29));
        r.advance_to(secs(2 * 86_400));
        assert_eq!((r.time().month, r.time().day), (3, 1));
        // 2023 is not: Feb 28 -> Mar 1.
        let mut r = at(2023, 2, 28, 0, 0, 0);
        r.advance_to(secs(86_400));
        assert_eq!((r.time().month, r.time().day), (3, 1));
        // 2000 is (divisible by 4 in the two-digit year).
        let mut r = at(2000, 2, 28, 0, 0, 0);
        r.advance_to(secs(86_400));
        assert_eq!((r.time().month, r.time().day), (2, 29));
        for (mo, len) in [
            (1, 31),
            (3, 31),
            (4, 30),
            (6, 30),
            (9, 30),
            (11, 30),
            (12, 31),
        ] {
            assert_eq!(days_in_month(1, mo), len, "month {mo}");
        }
    }

    #[test]
    fn year_end_and_century_wrap() {
        let mut r = at(2023, 12, 31, 23, 59, 59);
        r.advance_to(secs(1));
        let t = r.time();
        assert_eq!((t.year, t.month, t.day, t.hour), (2024, 1, 1, 0));
        let mut r = at(2099, 12, 31, 23, 59, 59);
        r.advance_to(secs(1));
        assert_eq!((r.time().year, r.time().month, r.time().day), (2000, 1, 1));
    }

    #[test]
    fn a_long_gap_matches_one_second_steps() {
        let mut a = at(2021, 6, 15, 8, 30, 20);
        let mut b = a.clone();
        let n = 3 * 86_400 + 5 * 3600 + 17; // not a round number
        for i in 1..=n {
            b.advance_to(secs(i));
        }
        a.advance_to(secs(n));
        assert_eq!(a, b);
    }

    #[test]
    fn weekday_of_known_dates() {
        assert_eq!(weekday_of(0, 1, 1), 6, "2000-01-01 was a Saturday");
        assert_eq!(weekday_of(24, 3, 9), 6, "2024-03-09 was a Saturday");
        assert_eq!(weekday_of(26, 11, 6), 5, "2026-11-06 is a Friday");
        assert_eq!(RtcTime::new(2001, 9, 9, 0, 0, 0).unwrap().weekday, 0);
    }

    #[test]
    fn every_rtc_time_is_a_real_date_and_the_clock_only_produces_them() {
        // Exhaustive over years and months, every day 0..=32: new() accepts
        // exactly the real days.
        for year in 1995..=2105u16 {
            for month in 0..=13u8 {
                for day in 0..=32u8 {
                    let real = (2000..=2099).contains(&year)
                        && (1..=12).contains(&month)
                        && (1..=days_in_month((year - 2000) as u8, month)).contains(&day);
                    assert_eq!(
                        RtcTime::new(year, month, day, 0, 0, 0).is_ok(),
                        real,
                        "{year}-{month}-{day}"
                    );
                }
            }
        }
        // Every accepted time sets the clock, which reads it back unchanged,
        // and the clock running on never leaves the valid set.
        let t = RtcTime::new(2099, 12, 31, 23, 59, 59).unwrap();
        let mut r = Rtc::power_on();
        r.set_time(t, 0);
        assert_eq!(r.time(), t);
        for day in 1..=40_000u64 {
            r.advance_to(day * 86_400 * CPS + day);
            let now = r.time();
            // The weekday free-runs (a game may have written any), so only the
            // date and time of day are checked.
            assert!(
                RtcTime::new(
                    now.year(),
                    now.month(),
                    now.day(),
                    now.hour(),
                    now.minute(),
                    now.second()
                )
                .is_ok(),
                "{now:?}"
            );
        }
    }

    #[test]
    fn rtc_time_rejects_impossible_dates() {
        assert!(RtcTime::new(1999, 1, 1, 0, 0, 0).is_err());
        assert!(RtcTime::new(2100, 1, 1, 0, 0, 0).is_err());
        assert!(RtcTime::new(2023, 2, 29, 0, 0, 0).is_err());
        assert!(RtcTime::new(2024, 2, 29, 0, 0, 0).is_ok());
        assert!(RtcTime::new(2024, 13, 1, 0, 0, 0).is_err());
        assert!(RtcTime::new(2024, 1, 1, 24, 0, 0).is_err());
        assert!(RtcTime::new(2024, 1, 1, 0, 60, 0).is_err());
        assert!(RtcTime::new(2024, 1, 1, 0, 0, 60).is_err());
    }

    #[test]
    fn twelve_and_twenty_four_hour_encodings() {
        let mut r = at(2024, 1, 1, 15, 4, 5);
        assert_eq!(r.read_time(), [0x15, 0x04, 0x05]);
        r.control = 0; // 12-hour mode
        assert_eq!(r.read_time(), [0x83, 0x04, 0x05], "3 PM");
        r.hour = 0;
        assert_eq!(r.read_time()[0], 0x00, "midnight is 12 AM = 0 AM");
        r.hour = 12;
        assert_eq!(r.read_time()[0], 0x80, "noon is 0 PM");
        r.hour = 11;
        assert_eq!(r.read_time()[0], 0x11);
    }

    #[test]
    fn writes_decode_by_mode_and_ignore_invalid_fields() {
        let mut r = at(2024, 1, 1, 0, 0, 0);
        r.write_time([0x23, 0x59, 0x30]);
        assert_eq!((r.hour, r.minute, r.second), (23, 59, 30));
        r.write_time([0x24, 0x60, 0x3A]);
        assert_eq!((r.hour, r.minute, r.second), (23, 59, 30), "all invalid");
        r.control = 0;
        r.write_time([0x85, 0x00, 0x00]);
        assert_eq!(r.hour, 17, "5 PM in 12-hour mode");
        r.write_time([0x12, 0x00, 0x00]);
        assert_eq!(r.hour, 17, "12 is not a 12-hour value");
        r.write_datetime([0x24, 0x02, 0x29, 3, 0x01, 0x02, 0x03]);
        let t = r.time();
        assert_eq!((t.year, t.month, t.day, t.weekday), (2024, 2, 29, 3));
        let before = r.time();
        r.write_datetime([0x24, 0x02, 0x30, 9, 0x00, 0x00, 0x00]);
        assert_eq!(r.time().day, before.day, "Feb 30 refused");
        assert_eq!(r.time().weekday, before.weekday);
    }

    #[test]
    fn control_register_masks_unwritable_bits() {
        let mut r = Rtc::power_on();
        r.write_control(0xFF);
        assert_eq!(r.control, 0x80 | 0x6A, "bit 7 is kept, not writable");
        r.write_control(0x00);
        assert_eq!(r.control, 0x80);
    }

    #[test]
    fn footer_round_trips_and_validates() {
        let mut r = at(2024, 2, 29, 13, 45, 59);
        r.advance_to(CPS / 3);
        let f = r.footer();
        assert_eq!(f.len(), FOOTER_LEN);
        assert_eq!(&f[..8], b"AGBRTC01");
        let back = Rtc::from_footer(&f, 777).unwrap();
        assert_eq!(back.synced_at, 777);
        assert_eq!(
            Rtc {
                synced_at: 0,
                ..back
            },
            Rtc { synced_at: 0, ..r }
        );
        let broken = |i: usize, v: u8| {
            let mut g = f;
            g[i] = v;
            Rtc::from_footer(&g, 0).is_err()
        };
        assert!(broken(0, b'X'), "magic");
        assert!(broken(8, 0x01), "control");
        assert!(broken(9, 0x9A), "bad BCD year");
        assert!(broken(10, 0x13), "month 13");
        assert!(broken(10, 0x00), "month 0");
        assert!(broken(11, 0x30), "Feb 30");
        assert!(broken(12, 7), "weekday");
        assert!(broken(13, 0x24), "hour");
        assert!(broken(14, 0x60), "minute");
        assert!(broken(15, 0x60), "second");
        assert!(broken(19, 0x01), "sub-second >= 2^24");
        assert!(broken(23, 1), "reserved");
        assert!(Rtc::from_footer(&f[..23], 0).is_err());
        assert!(Rtc::from_footer(&[f.as_slice(), &[0]].concat(), 0).is_err());
    }

    /// Writing the month or year alone must not leave the old day past the end of
    /// the new month (code-review ST#2): the day is clamped, so the clock holds a
    /// real date and a state taken from it loads.
    #[test]
    fn a_datetime_write_never_leaves_an_impossible_date() {
        let load_back = |r: &Rtc| {
            let mut w = StateWriter::new();
            r.save_state(&mut w);
            let bytes = w.into_bytes();
            let mut back = Rtc::power_on();
            back.load_state(&mut StateReader::new(&bytes))
                .expect("a reachable clock state loads");
            assert_eq!(&back, r);
        };
        // 31 January 2024 (leap year), then month = 02 with an unusable day byte.
        let mut r = Rtc::power_on();
        r.set_time(RtcTime::new(2024, 1, 31, 1, 2, 3).unwrap(), 0);
        r.write_datetime([0x24, 0x02, 0xFF, 9, 0x01, 0x02, 0x03]);
        assert_eq!((r.month, r.day), (2, 29));
        load_back(&r);
        // 29 February 2024, then the year becomes 2023.
        let mut r = Rtc::power_on();
        r.set_time(RtcTime::new(2024, 2, 29, 1, 2, 3).unwrap(), 0);
        r.write_datetime([0x23, 0x02, 0xFF, 9, 0x01, 0x02, 0x03]);
        assert_eq!((r.year, r.month, r.day), (23, 2, 28));
        load_back(&r);
        // A valid day in the same write is kept as written.
        r.write_datetime([0x23, 0x04, 0x30, 9, 0x01, 0x02, 0x03]);
        assert_eq!((r.month, r.day), (4, 30));
        load_back(&r);
    }

    type Case = (&'static str, fn(&mut Rtc));

    /// Every field `load_state` range-checks is refused when out of range, by name.
    #[test]
    fn a_state_with_an_impossible_clock_field_is_refused() {
        let mut ok = Rtc::power_on();
        ok.set_time(RtcTime::new(2024, 4, 30, 1, 2, 3).unwrap(), 0);
        let load = |r: &Rtc| {
            let mut w = StateWriter::new();
            r.save_state(&mut w);
            let bytes = w.into_bytes();
            Rtc::power_on().load_state(&mut StateReader::new(&bytes))
        };
        assert!(load(&ok).is_ok());
        let cases: [Case; 11] = [
            ("RTC control", |r| r.control = 0x01),
            ("RTC control", |r| r.control = 0x04),
            ("RTC year", |r| r.year = 100),
            ("RTC month", |r| r.month = 13),
            ("RTC month", |r| r.month = 0),
            ("RTC day", |r| r.day = 31), // April has 30
            ("RTC weekday", |r| r.weekday = 7),
            ("RTC time of day", |r| r.hour = 24),
            ("RTC sub-second count", |r| r.subsecond = CPS as u32),
            ("RTC sync time", |r| r.synced_at = crate::state::MAX_CLOCK),
            ("RTC time of day", |r| r.minute = 60),
        ];
        for (name, break_it) in cases {
            let mut bad = ok.clone();
            break_it(&mut bad);
            assert_eq!(load(&bad), Err(StateError::Invalid(name)), "{name}");
        }
        // The control bits a game can hold are accepted (deep-review #12).
        for control in [0x80, 0x82, 0x40, 0x6A | 0x80] {
            let mut r = ok.clone();
            r.control = control;
            assert!(load(&r).is_ok(), "{control:#x}");
        }
    }

    /// Whole 100-year cycles of the date cost nothing (deep-review #22): a
    /// clock far from its sync point reads the same date as one a cycle closer,
    /// and does not step a billion days to get there.
    #[test]
    fn a_huge_elapsed_time_skips_whole_calendar_cycles() {
        let mut base = Rtc::power_on();
        base.set_time(RtcTime::new(2023, 12, 31, 23, 59, 30).unwrap(), 0);
        let cycle_secs = DAYS_PER_CENTURY * 86_400;
        for extra in [0u64, 1, 45, 86_400 * 59 + 7, 86_400 * 1000] {
            let mut near = base.clone();
            near.add_seconds(extra);
            let mut far = base.clone();
            far.add_seconds(extra + 1_000_000_000 * cycle_secs);
            // The weekday is not part of the date cycle: 36 525 days are 5217
            // weeks + 6 days, so it moves by 6 per cycle.
            let weekday = (u64::from(near.weekday) + 1_000_000_000 * (DAYS_PER_CENTURY % 7)) % 7;
            near.weekday = weekday as u8;
            assert_eq!(far, near, "extra {extra}");
        }
    }
}
