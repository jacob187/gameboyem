//! What the app is showing and what moves it between screens. Pure logic: no egui, no
//! window and no files, so the rules are tested without a display.
//!
//! The app is in one [`Mode`]:
//!
//! ```text
//!            play a game                     Esc / P
//! Library ----------------> Playing ------------------> Paused
//!    ^                         ^   <------------------     |
//!    |                         |       Esc / P / Resume    |
//!    +-------------------------+---------------------------+
//!                          Back to library
//! ```
//!
//! A ROM dropped on the window starts playing from any mode. Only `Playing` runs the
//! emulator and passes keys to the game; the other two show a menu.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use emu_common::{FrameRef, Pixels};
use jiff::tz::TimeZone;

use crate::library::{Entry, GameId};
use crate::state::Snapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The game list. No game is running (the core is a parked placeholder).
    Library,
    /// A game is running and the window shows only the picture.
    Playing,
    /// A game is stopped, with the pause menu over its last picture.
    Paused,
}

impl Mode {
    /// The emulator advances, audio plays and saves are written on a timer.
    pub fn runs_emulation(self) -> bool {
        self == Mode::Playing
    }

    /// Keys press the game's buttons and trigger its snapshot, screenshot and
    /// fast-forward keys. Everywhere else they belong to the menu in front.
    pub fn game_input(self) -> bool {
        self == Mode::Playing
    }

    /// A screen is drawn over (or instead of) the picture.
    pub fn menu_visible(self) -> bool {
        self != Mode::Playing
    }

    /// Esc or P while playing opens the pause menu; from any other mode nothing changes.
    pub fn pause(self) -> Mode {
        match self {
            Mode::Playing => Mode::Paused,
            other => other,
        }
    }

    /// Resume from the pause menu.
    pub fn resume(self) -> Mode {
        match self {
            Mode::Paused => Mode::Playing,
            other => other,
        }
    }

    /// A game was started (from the library, a drop, or the command line).
    pub fn play(self) -> Mode {
        Mode::Playing
    }

    /// Leave the game for the game list.
    pub fn back_to_library(self) -> Mode {
        Mode::Library
    }
}

/// What Esc does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscapeEffect {
    /// Close the program (the library, as Esc always did).
    Quit,
    /// Open the pause menu.
    Pause,
    /// Close the pause menu and carry on.
    Resume,
    /// Close the help screen or a confirmation in front of the menu.
    CloseOverlay,
}

/// The effect of Esc in `mode`, with a help screen or confirmation open or not.
/// `can_pause` is false where there is no game to pause (`--test-pattern`): Esc quits
/// there, as it always did.
pub fn escape_effect(mode: Mode, overlay_open: bool, can_pause: bool) -> EscapeEffect {
    if overlay_open && mode.menu_visible() {
        return EscapeEffect::CloseOverlay;
    }
    match mode {
        Mode::Library => EscapeEffect::Quit,
        Mode::Playing if can_pause => EscapeEffect::Pause,
        Mode::Playing => EscapeEffect::Quit,
        Mode::Paused => EscapeEffect::Resume,
    }
}

/// What the game list is ordered by.
#[derive(Clone, Copy, Debug)]
pub struct SortKey<'a> {
    pub last_played: Option<u64>,
    pub title: &'a str,
    pub id: &'a GameId,
}

impl<'a> From<&'a Entry> for SortKey<'a> {
    fn from(e: &'a Entry) -> Self {
        Self {
            last_played: e.last_played,
            title: &e.title,
            id: &e.id,
        }
    }
}

/// The order games are listed in: most recently played first, never played after
/// those, then by title (ignoring case), then by id so the order is total. Returns
/// indices into `keys`.
pub fn sort_order(keys: &[SortKey<'_>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by(|&a, &b| {
        let (a, b) = (&keys[a], &keys[b]);
        b.last_played
            .cmp(&a.last_played)
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
            .then_with(|| a.id.cmp(b.id))
    });
    order
}

/// [`sort_order`] of the library's entries.
pub fn library_order(entries: &[Entry]) -> Vec<usize> {
    let keys: Vec<SortKey<'_>> = entries.iter().map(SortKey::from).collect();
    sort_order(&keys)
}

/// Emulated frames, at normal speed, before the first try at a thumbnail (10 s at 60
/// frames a second).
pub const THUMBNAIL_FRAMES: u64 = 600;

/// Frames between tries when the screen was a single colour.
pub const THUMBNAIL_RETRY: u64 = 30;

/// Frames after which no more tries are made (a minute): some games never show more.
pub const THUMBNAIL_GIVE_UP: u64 = 3600;

/// Decides when to try for the library thumbnail of the running game: from
/// [`THUMBNAIL_FRAMES`] on, every [`THUMBNAIL_RETRY`] frames until the app has taken
/// one ([`ThumbnailTrigger::done`]) or [`THUMBNAIL_GIVE_UP`] is reached.
#[derive(Debug, Default)]
pub struct ThumbnailTrigger {
    armed: bool,
    frames: u64,
}

impl ThumbnailTrigger {
    /// A trigger for a game that is `wanted` (it is in the library and has no
    /// thumbnail). An unwanted one never fires.
    pub fn new(wanted: bool) -> Self {
        Self {
            armed: wanted,
            frames: 0,
        }
    }

    /// Count one emulated frame at normal speed. True when the app should try now.
    pub fn frame(&mut self) -> bool {
        if !self.armed {
            return false;
        }
        self.frames += 1;
        if self.frames > THUMBNAIL_GIVE_UP {
            self.armed = false;
            return false;
        }
        self.frames >= THUMBNAIL_FRAMES
            && (self.frames - THUMBNAIL_FRAMES).is_multiple_of(THUMBNAIL_RETRY)
    }

    /// The thumbnail was taken: no more tries.
    pub fn done(&mut self) {
        self.armed = false;
    }
}

/// Is the whole picture one colour? Such a frame (a black screen between scenes) makes
/// a poor thumbnail.
pub fn frame_is_uniform(frame: &FrameRef<'_>) -> bool {
    match frame.pixels {
        Pixels::Shades(px) => px.windows(2).all(|w| w[0] & 3 == w[1] & 3),
        Pixels::Rgb555(px) => px.windows(2).all(|w| w[0] & 0x7FFF == w[1] & 0x7FFF),
    }
}

/// How long a toast stays on screen.
pub const TOAST_DURATION: Duration = Duration::from_secs(5);

/// The most toasts shown at once; a newer one pushes out the oldest.
const MAX_TOASTS: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub level: log::Level,
    pub text: String,
    pub until: Instant,
}

/// The short messages shown over the app: errors, warnings and results of library
/// actions.
#[derive(Debug, Default)]
pub struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    /// Show `text` until `now + TOAST_DURATION`. The same text again just stays longer.
    pub fn push(&mut self, level: log::Level, text: impl Into<String>, now: Instant) {
        let text = text.into();
        let until = now + TOAST_DURATION;
        if let Some(t) = self.items.iter_mut().find(|t| t.text == text) {
            t.until = until;
            t.level = level;
            return;
        }
        if self.items.len() == MAX_TOASTS {
            self.items.remove(0);
        }
        self.items.push(Toast { level, text, until });
    }

    /// Drop the toasts that have run out; returns when the next one will, if any.
    pub fn expire(&mut self, now: Instant) -> Option<Instant> {
        self.items.retain(|t| t.until > now);
        self.items.iter().map(|t| t.until).min()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Toast> {
        self.items.iter()
    }
}

/// What the player asked for on a screen, collected while it is drawn and carried out
/// afterwards (a dialog or a file write must not run in the middle of drawing).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiAction {
    /// Play this library game.
    Play(GameId),
    /// Open the file dialog to add games.
    AddGames,
    /// Take the game out of the library, and its saves too if asked.
    Remove {
        id: GameId,
        delete_saves: bool,
    },
    /// Discard the game's resume point and power it on normally, keeping its own save.
    StartFresh(GameId),
    /// Open the app data folder in the file manager.
    OpenDataFolder,
    Resume,
    /// Add a snapshot of the running game.
    SaveSnapshot,
    /// Load the snapshot taken at this time (Unix seconds).
    LoadSnapshot(u64),
    /// Delete the snapshot taken at this time.
    DeleteSnapshot(u64),
    /// Power-cycle the running game; its own save is kept.
    Restart,
    Screenshot,
    BackToLibrary,
    Quit,
}

/// One row of the snapshot list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotItem {
    /// When it was taken (Unix seconds); what the buttons act on.
    pub secs: u64,
    /// That time as the player reads it, from [`friendly_time`].
    pub when: String,
    /// The picture, if it has one.
    pub image: Option<PathBuf>,
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `secs` (Unix seconds) as a person says it, in the zone `tz`, relative to `now`:
/// "Today 5:41 PM", "Yesterday 9:03 PM", then "Nov 3, 9:03 PM" (with the year when it is not
/// this year's).
pub fn friendly_time(secs: u64, now: u64, tz: &TimeZone) -> String {
    let zoned = |secs: u64| {
        i64::try_from(secs)
            .ok()
            .and_then(|s| jiff::Timestamp::from_second(s).ok())
            .map(|t| t.to_zoned(tz.clone()))
    };
    let (Some(at), Some(today)) = (zoned(secs), zoned(now)) else {
        return "Unknown time".to_owned();
    };
    let (hour, minute) = (at.hour(), at.minute());
    let clock = format!(
        "{}:{minute:02} {}",
        match hour % 12 {
            0 => 12,
            h => h,
        },
        if hour < 12 { "AM" } else { "PM" }
    );
    let (date, today) = (at.date(), today.date());
    if date == today {
        format!("Today {clock}")
    } else if today.yesterday().is_ok_and(|y| y == date) {
        format!("Yesterday {clock}")
    } else {
        let month = MONTHS[usize::try_from(date.month() - 1).unwrap_or(0).min(11)];
        let year = if date.year() == today.year() {
            String::new()
        } else {
            format!(", {}", date.year())
        };
        format!("{month} {}{year}, {clock}", date.day())
    }
}

/// The snapshot list rows for `list`, with their times worded for the computer's own time
/// zone and clock.
pub fn snapshot_items(list: Vec<Snapshot>) -> Vec<SnapshotItem> {
    let (now, tz) = (crate::library::now_secs(), TimeZone::system());
    list.into_iter()
        .map(|s| SnapshotItem {
            secs: s.secs,
            when: friendly_time(s.secs, now, &tz),
            image: s.image,
        })
        .collect()
}

/// [`friendly_time`] for the computer's own time zone and clock.
pub fn friendly_time_local(secs: u64) -> String {
    friendly_time(secs, crate::library::now_secs(), &TimeZone::system())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sort keys for `(title, hash, last_played)` games, with the ids they own.
    fn ids(games: &[(&str, u64, Option<u64>)]) -> Vec<GameId> {
        games.iter().map(|&(t, h, _)| GameId::new(t, h)).collect()
    }

    fn keys<'a>(games: &[(&'a str, u64, Option<u64>)], ids: &'a [GameId]) -> Vec<SortKey<'a>> {
        games
            .iter()
            .zip(ids)
            .map(|(&(title, _, last_played), id)| SortKey {
                last_played,
                title,
                id,
            })
            .collect()
    }

    fn secs(text: &str) -> u64 {
        text.parse::<jiff::Timestamp>().unwrap().as_second() as u64
    }

    fn est() -> TimeZone {
        TimeZone::fixed(jiff::tz::offset(-5))
    }

    #[test]
    fn times_read_as_today_yesterday_then_a_date() {
        let now = secs("2026-11-07T17:54:00-05:00");
        let said = |at: &str| friendly_time(secs(at), now, &est());
        assert_eq!(said("2026-11-07T17:41:00-05:00"), "Today 5:41 PM");
        assert_eq!(said("2026-11-06T21:03:00-05:00"), "Yesterday 9:03 PM");
        assert_eq!(said("2026-11-07T00:05:00-05:00"), "Today 12:05 AM");
        assert_eq!(said("2026-11-06T12:00:00-05:00"), "Yesterday 12:00 PM");
        assert_eq!(said("2026-11-03T21:03:00-05:00"), "Nov 3, 9:03 PM");
        assert_eq!(said("2026-03-02T09:03:00-05:00"), "Mar 2, 9:03 AM");
        assert_eq!(said("2025-12-31T23:59:00-05:00"), "Dec 31, 2025, 11:59 PM");
    }

    #[test]
    fn today_is_the_players_day_not_utc() {
        // 01:00 UTC on the 8th is still the evening of the 7th in New York.
        let now = secs("2026-11-08T01:00:00Z");
        let at = secs("2026-11-07T23:30:00Z");
        assert_eq!(friendly_time(at, now, &est()), "Today 6:30 PM");
        assert_eq!(friendly_time(at, now, &TimeZone::UTC), "Yesterday 11:30 PM");
        // The clock's own day boundary.
        let after_midnight = secs("2026-11-07T00:01:00-05:00");
        let before = secs("2026-11-06T23:59:00-05:00");
        let now = secs("2026-11-07T08:00:00-05:00");
        assert_eq!(friendly_time(after_midnight, now, &est()), "Today 12:01 AM");
        assert_eq!(friendly_time(before, now, &est()), "Yesterday 11:59 PM");
    }

    #[test]
    fn a_time_from_the_future_is_still_a_date() {
        let now = secs("2026-11-07T17:54:00-05:00");
        let later = secs("2026-11-09T08:00:00-05:00");
        assert_eq!(friendly_time(later, now, &est()), "Nov 9, 8:00 AM");
        assert_eq!(friendly_time(u64::MAX, now, &est()), "Unknown time");
    }

    #[test]
    fn a_game_goes_library_playing_paused_and_back() {
        let mut mode = Mode::Library;
        mode = mode.play();
        assert_eq!(mode, Mode::Playing);
        mode = mode.pause();
        assert_eq!(mode, Mode::Paused);
        mode = mode.resume();
        assert_eq!(mode, Mode::Playing);
        mode = mode.pause().back_to_library();
        assert_eq!(mode, Mode::Library);
    }

    #[test]
    fn pause_and_resume_only_apply_where_they_make_sense() {
        assert_eq!(Mode::Library.pause(), Mode::Library);
        assert_eq!(Mode::Paused.pause(), Mode::Paused);
        assert_eq!(Mode::Library.resume(), Mode::Library);
        assert_eq!(Mode::Playing.resume(), Mode::Playing);
    }

    #[test]
    fn a_drop_starts_a_game_from_every_mode() {
        for mode in [Mode::Library, Mode::Playing, Mode::Paused] {
            assert_eq!(mode.play(), Mode::Playing);
        }
    }

    #[test]
    fn only_playing_runs_the_game_and_takes_its_keys() {
        assert!(Mode::Playing.runs_emulation() && Mode::Playing.game_input());
        assert!(!Mode::Playing.menu_visible());
        for mode in [Mode::Library, Mode::Paused] {
            assert!(!mode.runs_emulation(), "{mode:?}");
            assert!(!mode.game_input(), "{mode:?}");
            assert!(mode.menu_visible(), "{mode:?}");
        }
    }

    #[test]
    fn esc_depends_on_where_the_app_is() {
        assert_eq!(
            escape_effect(Mode::Library, false, true),
            EscapeEffect::Quit
        );
        assert_eq!(
            escape_effect(Mode::Playing, false, true),
            EscapeEffect::Pause
        );
        assert_eq!(
            escape_effect(Mode::Paused, false, true),
            EscapeEffect::Resume
        );
    }

    #[test]
    fn esc_quits_where_there_is_no_game_to_pause() {
        assert_eq!(
            escape_effect(Mode::Playing, false, false),
            EscapeEffect::Quit
        );
        assert_eq!(
            escape_effect(Mode::Library, false, false),
            EscapeEffect::Quit
        );
        // An open menu is still closed by Esc.
        assert_eq!(
            escape_effect(Mode::Paused, false, false),
            EscapeEffect::Resume
        );
    }

    #[test]
    fn esc_closes_the_help_or_a_confirmation_before_anything_else() {
        assert_eq!(
            escape_effect(Mode::Library, true, true),
            EscapeEffect::CloseOverlay
        );
        assert_eq!(
            escape_effect(Mode::Paused, true, true),
            EscapeEffect::CloseOverlay
        );
    }

    #[test]
    fn the_list_is_most_recently_played_first_then_by_title() {
        let games = [
            ("Zelda", 1, None),
            ("apple", 2, None),
            ("Mario", 3, Some(100)),
            ("Tetris", 4, Some(200)),
            ("Banana", 5, None),
        ];
        let ids = ids(&games);
        let titles: Vec<&str> = sort_order(&keys(&games, &ids))
            .into_iter()
            .map(|i| games[i].0)
            .collect();
        assert_eq!(titles, ["Tetris", "Mario", "apple", "Banana", "Zelda"]);
    }

    #[test]
    fn games_with_one_title_keep_a_stable_order() {
        let games = [("Same", 9, None), ("Same", 3, None)];
        let ids = ids(&games);
        // However the list arrives, the game with the lower id is first.
        assert_eq!(sort_order(&keys(&games, &ids)), [1, 0]);
        let swapped = [games[1], games[0]];
        let swapped_ids = [ids[1].clone(), ids[0].clone()];
        assert_eq!(sort_order(&keys(&swapped, &swapped_ids)), [0, 1]);
    }

    #[test]
    fn an_empty_library_has_an_empty_order() {
        assert!(library_order(&[]).is_empty());
    }

    #[test]
    fn the_first_try_is_at_the_threshold() {
        let mut t = ThumbnailTrigger::new(true);
        for frame in 1..THUMBNAIL_FRAMES {
            assert!(!t.frame(), "tried early at frame {frame}");
        }
        assert!(t.frame(), "tries on frame {THUMBNAIL_FRAMES}");
    }

    #[test]
    fn it_captures_once_and_then_stops() {
        let mut t = ThumbnailTrigger::new(true);
        for _ in 1..THUMBNAIL_FRAMES {
            t.frame();
        }
        assert!(t.frame());
        t.done();
        for _ in 0..(THUMBNAIL_GIVE_UP * 2) {
            assert!(!t.frame(), "fires only once");
        }
    }

    #[test]
    fn a_failed_try_is_repeated_until_the_limit() {
        let mut t = ThumbnailTrigger::new(true);
        let mut tries = Vec::new();
        for frame in 1..=(THUMBNAIL_GIVE_UP + 100) {
            if t.frame() {
                tries.push(frame);
            }
        }
        assert_eq!(tries[0], THUMBNAIL_FRAMES);
        assert_eq!(tries[1], THUMBNAIL_FRAMES + THUMBNAIL_RETRY);
        assert!(tries.iter().all(|&f| f <= THUMBNAIL_GIVE_UP));
        assert_eq!(
            *tries.last().unwrap(),
            THUMBNAIL_FRAMES
                + (THUMBNAIL_GIVE_UP - THUMBNAIL_FRAMES) / THUMBNAIL_RETRY * THUMBNAIL_RETRY
        );
    }

    #[test]
    fn a_game_with_a_thumbnail_never_fires() {
        let mut t = ThumbnailTrigger::new(false);
        for _ in 0..(THUMBNAIL_GIVE_UP * 2) {
            assert!(!t.frame());
        }
        assert!(!ThumbnailTrigger::default().frame());
    }

    #[test]
    fn the_threshold_is_about_ten_seconds() {
        assert_eq!(THUMBNAIL_FRAMES, 600);
    }

    #[test]
    fn a_single_colour_frame_is_uniform() {
        let flat = [2u8; 160 * 144];
        let frame = |px| FrameRef {
            width: 160,
            height: 144,
            pixels: px,
        };
        assert!(frame_is_uniform(&frame(Pixels::Shades(&flat))));
        let mut busy = flat;
        busy[160 * 144 - 1] = 1;
        assert!(!frame_is_uniform(&frame(Pixels::Shades(&busy))));

        let flat = [0x1234u16; 240 * 160];
        let wide = |px| FrameRef {
            width: 240,
            height: 160,
            pixels: px,
        };
        assert!(frame_is_uniform(&wide(Pixels::Rgb555(&flat))));
        let mut busy = flat;
        busy[7] = 0x1235;
        assert!(!frame_is_uniform(&wide(Pixels::Rgb555(&busy))));
    }

    #[test]
    fn toasts_expire_and_report_the_next_expiry() {
        let t0 = Instant::now();
        let mut toasts = Toasts::default();
        assert!(toasts.is_empty());
        assert_eq!(toasts.expire(t0), None);
        toasts.push(log::Level::Error, "first", t0);
        toasts.push(log::Level::Warn, "second", t0 + Duration::from_secs(2));
        assert_eq!(toasts.iter().count(), 2);
        assert_eq!(
            toasts.expire(t0 + Duration::from_secs(1)),
            Some(t0 + TOAST_DURATION)
        );
        let after_first = t0 + TOAST_DURATION;
        assert_eq!(
            toasts.expire(after_first),
            Some(t0 + Duration::from_secs(2) + TOAST_DURATION)
        );
        assert_eq!(toasts.iter().count(), 1);
        assert_eq!(toasts.expire(t0 + Duration::from_secs(60)), None);
        assert!(toasts.is_empty());
    }

    #[test]
    fn the_same_message_is_shown_once_and_stays_longer() {
        let t0 = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push(log::Level::Warn, "bad ROM", t0);
        toasts.push(log::Level::Warn, "bad ROM", t0 + Duration::from_secs(3));
        assert_eq!(toasts.iter().count(), 1);
        assert_eq!(
            toasts.expire(t0 + Duration::from_secs(4)),
            Some(t0 + Duration::from_secs(3) + TOAST_DURATION)
        );
    }

    #[test]
    fn only_the_newest_toasts_are_kept() {
        let t0 = Instant::now();
        let mut toasts = Toasts::default();
        for n in 0..10 {
            toasts.push(log::Level::Info, format!("message {n}"), t0);
        }
        let texts: Vec<&str> = toasts.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["message 6", "message 7", "message 8", "message 9"]);
    }
}
