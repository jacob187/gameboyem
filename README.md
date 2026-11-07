# gameboyem

A Game Boy, Game Boy Color and Game Boy Advance emulator written in Rust. One app, `gb`, plays all three on macOS, Windows and Linux.

- **Game Boy / Color:** accurate CPU, graphics, sound and timers; MBC1, MBC3 (with clock) and MBC5 cartridges.
- **Game Boy Advance:** ARM7TDMI CPU, all graphics modes, sound, DMA, timers, and SRAM/Flash/EEPROM saves with the cartridge clock. A built-in BIOS is included, so no BIOS file is needed.
- **For every system:** battery saves, resume and snapshots, fast-forward, screenshots, and input recording with replay.

## Download

Get the latest build from the [Releases page](https://github.com/jacob187/gameboyem/releases). You don't need Rust.

| System | File | Install |
|--------|------|---------|
| macOS | `.dmg` (`arm64` for Apple silicon, `x86_64` for Intel) | Drag gameboyem to Applications |
| Windows | `-setup.exe`, or `-portable.zip` | Run the installer, or unzip and run `gb.exe` |
| Linux | `.AppImage`, `.deb` or `.tar.gz` | `chmod +x` and run the AppImage, or `sudo apt install ./<file>.deb` |

The builds aren't code-signed yet, so the first launch needs one extra step:

- **macOS:** right-click the app, choose **Open**, then **Open** again.
- **Windows:** at "Windows protected your PC", click **More info**, then **Run anyway**.

The app opens on your game library. The five homebrew games from `homebrew/` (Snake, Breakout, cgbdemo, GRAINS and MOTH) come bundled and are already there on first launch (with the portable zip or the `.tar.gz`, keep the `games` folder next to the program). Click **Add game** to add your own ROMs (`.gb`, `.gbc` or `.gba`), or drag one onto the window; click a game to play and press **Esc** for the menu. No commercial games are included; see [Legal](#legal).

## Controls

| Button | Keys |
|--------|------|
| D-pad | W A S D or arrow keys |
| A / B | E or X / Z |
| L / R (GBA) | Q / R |
| Start / Select | Enter / Backspace |
| Save snapshot / load the latest snapshot | K / L |
| Pause / fast-forward | P / hold Tab |
| Screenshot | C |
| Pause menu (resume, snapshots, restart, back to library, help) | Esc or P (Esc quits from the library) |

## Saves

Everything you produce is kept in one folder per user, never next to the ROM:

| OS | Folder |
|----|--------|
| macOS | `~/Library/Application Support/gameboyem` |
| Windows | `%APPDATA%\gameboyem\data` |
| Linux | `~/.local/share/gameboyem` (or `$XDG_DATA_HOME/gameboyem`) |

Set `GAMEBOYEM_DATA_DIR` to use another folder. Inside it, `saves/`, `states/` and `screenshots/` hold your files, `games/` holds the game library, and `logs/gameboyem.log` is the log (three files of up to 1 MB).

There are three kinds of saving:

- **The game's own save** happens when the game saves, for example at a save point. It is kept automatically, in `saves/`.
- **Resume:** when you leave a game (back to the library, quit, or close the window), the app remembers exactly where you were, and the next time you open the game it picks up there ("Resumed where you left off"). It is also refreshed every minute while you play, so a crash costs at most a minute. Games with a resume point show a **Resume** tag in the library; right-click a game and choose **Start fresh** to forget it and start from the beginning (the game's own save is kept). There is no resume with `--record` or `--test-pattern`.
- **Snapshots:** save anywhere with **K** or **Save snapshot** in the pause menu, and load with **L** (the latest) or **Load snapshot…** in the menu (pick any, newest first, each with its picture and time; you can delete them there). A game keeps up to 50, and the oldest is removed to make room. **Restart game** in the menu starts the game over from power-on; its own save is kept.

The resume point and the snapshots are in `states/<game>/` (`resume.state`, `snap-<time>.state` and `snap-<time>.png`); screenshots (**C**) are in `screenshots/`. A resume point that can't be loaded (corrupt, or from other options) is kept as `resume.state.bad` and the game starts normally.

- Files are named after the game and a hash of the ROM's contents, so renaming or moving a ROM keeps its saves.
- **Existing saves are imported once.** The first time a ROM is opened, a `<rom>.sav` next to it is copied into the folder if there isn't one there yet, and old numbered save states (`<rom>.ss1`…`.ss9` next to the ROM, or `states/<game>.ss1`…`.ss9` from earlier versions) become snapshots, dated by their file's modified time and with no picture. The originals beside the ROM are left alone.
- `gb game.gb` plays that file without adding it to the library; `gb --add game.gb` copies a ROM into the library and exits.
- **Games with a clock** (Pokémon Ruby/Sapphire/Emerald, MBC3 games) keep time in emulated time. To start a new GBA game's clock at today's date, run `gb --rtc-start "$(date +%Y-%m-%dT%H:%M:%S)" game.gba`.

## Command line

```
gb [options] [rom]
  --scale N         window scale, 1-8 (default 4)
  --mute            no audio
  --system gb|gba   force the system (default: detected from the file)
  --model auto|dmg|cgb   Game Boy model (default: Color for colour games)
  --palette green|grey   original Game Boy palette
  --bios FILE       boot through your own GBA BIOS dump (optional)
  --save-type NAME  force the GBA save chip if a game saves wrongly
  --record FILE     record your inputs for exact replay
  --add             copy the ROM into the game library and exit
```

Run `gb --help` for the full list.

## Build from source

You need Rust 1.92 or newer. On Linux, install the windowing and audio packages first:

```sh
sudo apt-get install -y pkg-config libx11-dev libxkbcommon-dev libwayland-dev libasound2-dev libudev-dev
```

Then:

```sh
cargo build --release
./target/release/gb path/to/game.gba
```

A build run from `target/` has no bundled games (it does not use `roms/homebrew`). To try them, build the homebrew (see `homebrew/README.md`) and set `GAMEBOYEM_BUNDLED_DIR=roms/homebrew` (a developer override; it must be an existing folder, otherwise it is ignored with a warning).

The code is split into small crates:
- `gb-core` and `gba-core`: emulation only, with no platform code.
- `emu-common`: the shared pieces.
- `gb-frontend`: the window and audio app.
- `gb-test`: a headless test-ROM harness.

## Testing

The emulator is checked against public test ROMs: Blargg, Mooneye, dmg-acid2 and cgb-acid2 for the Game Boy, and jsmolka's gba-tests, FuzzARM, SingleStepTests and the tonc demos for the GBA. Our own homebrew games are checked too. The scripts download the ROMs; none are stored in the repo.

```sh
./scripts/fetch-test-roms.sh && ./scripts/fetch-gba-test-roms.sh
cargo test --workspace            # unit tests and one test per ROM
cargo run -p gb-test --release    # pass/fail table
```

All Game Boy and GBA suites pass except a few known Game Boy Color edge cases: AGE speed-switch and some `cgb_sound` ROMs. Those expected failures are listed in `crates/gb-test/expected.txt`, and CI fails if any result changes.

## Homebrew

`homebrew/` contains original games written for this emulator:
- Snake, Breakout and a colour test card for the Game Boy, written in C with GBDK.
- GRAINS (a falling-sand toy) and MOTH (a night-sky collector) for the GBA, written in Rust with [agb](https://github.com/agbrs/agb).

See `homebrew/README.md` and `homebrew/gba/README.md` to build them.

## Known limitations

- Commercial games aren't tested automatically. Most well-behaved games should run, but timing-sensitive ones may glitch.
- GBA: link cable and multiplayer aren't emulated. The few games that use the BIOS's own sound driver play no music. Some bus-timing and mid-scanline effects are approximated.
- Game Boy Color: a few speed-switch and sound edge cases differ from hardware. DMG games aren't colourised.
- No debugger, rewind or shaders.

## Legal

This project is not affiliated with or endorsed by Nintendo. Game Boy, Game Boy Color and Game Boy Advance are trademarks of Nintendo. No commercial games, BIOS images or other Nintendo files are included or downloaded; the only games bundled are the project's own homebrew, under the MIT licence. Use ROMs you've obtained legally, such as dumps of cartridges you own. Test ROMs are fetched from their authors under their own licences.

Released under the [MIT licence](LICENSE).
