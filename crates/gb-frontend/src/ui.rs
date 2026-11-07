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

use std::time::{Duration, Instant};

use crate::library::{Entry, GameId};

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

    /// Keys press the game's buttons and trigger its save-state, screenshot and
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
pub fn escape_effect(mode: Mode, overlay_open: bool) -> EscapeEffect {
    if overlay_open && mode.menu_visible() {
        return EscapeEffect::CloseOverlay;
    }
    match mode {
        Mode::Library => EscapeEffect::Quit,
        Mode::Playing => EscapeEffect::Pause,
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

/// Emulated frames a game runs before its picture becomes its thumbnail (10 s at
/// 60 frames a second).
pub const THUMBNAIL_FRAMES: u64 = 600;

/// Decides when to take the library thumbnail of the running game: once, when it has
/// run [`THUMBNAIL_FRAMES`] frames and has none yet.
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

    /// Count one emulated frame. True exactly once: on the frame that reaches the
    /// threshold.
    pub fn frame(&mut self) -> bool {
        if !self.armed {
            return false;
        }
        self.frames += 1;
        if self.frames >= THUMBNAIL_FRAMES {
            self.armed = false;
            return true;
        }
        false
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
    /// Open the app data folder in the file manager.
    OpenDataFolder,
    Resume,
    SelectSlot(u8),
    SaveState,
    LoadState,
    Screenshot,
    BackToLibrary,
    Quit,
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
        assert_eq!(escape_effect(Mode::Library, false), EscapeEffect::Quit);
        assert_eq!(escape_effect(Mode::Playing, false), EscapeEffect::Pause);
        assert_eq!(escape_effect(Mode::Paused, false), EscapeEffect::Resume);
    }

    #[test]
    fn esc_closes_the_help_or_a_confirmation_before_anything_else() {
        assert_eq!(
            escape_effect(Mode::Library, true),
            EscapeEffect::CloseOverlay
        );
        assert_eq!(
            escape_effect(Mode::Paused, true),
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
    fn the_thumbnail_is_taken_once_at_the_threshold() {
        let mut t = ThumbnailTrigger::new(true);
        for frame in 1..THUMBNAIL_FRAMES {
            assert!(!t.frame(), "fired early at frame {frame}");
        }
        assert!(t.frame(), "fires on frame {THUMBNAIL_FRAMES}");
        for _ in 0..(THUMBNAIL_FRAMES * 2) {
            assert!(!t.frame(), "fires only once");
        }
    }

    #[test]
    fn a_game_with_a_thumbnail_never_fires() {
        let mut t = ThumbnailTrigger::new(false);
        for _ in 0..(THUMBNAIL_FRAMES * 2) {
            assert!(!t.frame());
        }
        assert!(!ThumbnailTrigger::default().frame());
    }

    #[test]
    fn the_threshold_is_about_ten_seconds() {
        assert_eq!(THUMBNAIL_FRAMES, 600);
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
