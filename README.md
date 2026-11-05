# gb: a Game Boy and Game Boy Color emulator in Rust

A cycle-accurate-enough original Game Boy (DMG) emulator with Game Boy Color (CGB) support, plus two DMG homebrew games and a CGB colour test card written for it.

| Crate | What it is |
|-------|------------|
| `crates/gb-core` | Pure emulation of the DMG and CGB. It has no platform deps, no I/O, no `unsafe`, and runs deterministically. Builds anywhere, including `wasm32`. |
| `crates/gb-frontend` | The `gb` desktop binary (winit + pixels window, cpal audio). This is the only platform-specific crate. |
| `crates/gb-test` | A headless test-ROM harness. Every ROM is a `cargo test` case, and a table runner prints the results. |
| `homebrew/` | Snake and Breakout (DMG) and `cgbdemo` (a CGB-only colour test card) in C (GBDK-2020). |

## Build and run

You need Rust stable. `gb-frontend` needs ≥ 1.88; `gb-core` and `gb-test` need ≥ 1.85.

**macOS (arm64/x86_64).** No extra system packages are needed.

```sh
cargo build --release
./target/release/gb path/to/game.gb
```

**Linux (X11 and Wayland).** Install these packages first:

```sh
sudo apt-get install -y pkg-config libx11-dev libxkbcommon-dev libwayland-dev libasound2-dev libudev-dev
cargo build --release
./target/release/gb path/to/game.gb
```

**Windows.** Not yet built or run on Windows. CI has a non-blocking `windows-latest` job, but it hasn't been pushed and run yet. Locally, `cargo clippy --workspace --all-targets --target x86_64-pc-windows-msvc` type-checks clean.

**Command-line options:**

```
usage: gb [options] [--] <rom.gb|rom.gbc|rom.gba>
  --scale N        integer window scale in logical points, 1..=8 (default 4)
  --mute           disable audio output
  --system NAME    gb or gba (default: a .gba file, or a ROM with a GBA header, is a GBA game)
  --palette NAME   green (default) or grey (Game Boy only; colour games use their own colours)
  --model NAME     auto (default: Game Boy Color for CGB-flagged ROMs, else DMG), dmg or cgb (Game Boy only)
  --bios FILE      boot through a real 16 KiB GBA BIOS dump (GBA only; default: built-in HLE BIOS)
  --test-pattern   show a synthetic test pattern and tone instead of running the core
  --record FILE    record every input to FILE (must not exist) for `gb-test --replay`
  -h, --help       show this help
  --               end of options; the next argument is the ROM path, even if it starts with `-`
```

`.gb` and `.gbc` ROMs are both accepted (the extension isn't checked; the header decides). With `--model auto`, a ROM whose header byte 0x143 is 0x80 or 0xC0 runs as a Game Boy Color, in colour. Any other ROM runs as a DMG. `--model cgb` forces a Game Boy Color, and a ROM without a CGB flag then runs in DMG-compatibility mode. It isn't colourised, because there's no boot ROM to pick a compatibility palette, so it uses `--palette`. `--model dmg` runs everything on a DMG, including CGB ROMs.

Battery RAM is saved to `<rom>.sav` next to the ROM. The save is written atomically every 5 s when RAM changes, on pause, on quit, and on SIGINT/SIGTERM/SIGHUP (Ctrl-C). If writing fails, a warning is printed at once and then at most once a minute, and the failure is always reported on exit.

If an existing `<rom>.sav` can't be read, isn't a regular file, is larger than any valid save (128 KiB + 48 bytes), or is a wrong-size save that couldn't be copied to a backup, it's left untouched. Saving is then disabled for that session, with a warning at startup and "progress NOT saved" on exit. A wrong-size save that can be copied is backed up to `<rom>.sav.bak` (then `.bak1`, ...) before it's replaced. For MBC3 carts with a clock, periodic and pause saves happen only when the game changes cart RAM or an RTC register. The running clock is written on quit.

## Save states

Save states snapshot the whole machine, so you can save and resume anywhere, not just at a game's own save point. Press **K** to save to the current slot and **L** to load it. **1**–**9** pick the slot (default 1). Slot *n* is stored as `<rom>.ss<n>` next to the ROM and written atomically. The window title briefly shows the result, which is also printed to stderr.

A state is tied to the exact ROM and the console model (DMG or Game Boy Color) it was taken with. A state from another ROM or model, a damaged file, or one from an incompatible version is rejected, and the running game is left untouched. States from before CGB support (format version 1) are DMG states and still load on a DMG. Because `--model auto` now starts CGB-flagged ROMs (header 0x143 = 0x80 or 0xC0) as a Game Boy Color, such a ROM refuses an old state with a message pointing to `--model dmg`; start it with that flag to load the state.

Loading a state also rewinds battery RAM, and the next autosave writes that RAM over `<rom>.sav`. So before the first load in a session, the existing `<rom>.sav` is copied to `<rom>.sav.bak` (then `.bak1`, ...). If that copy fails, the load is refused.

## Controls

| Game Boy | Keyboard |
|----------|----------|
| D-pad | Arrow keys or W / A / S / D |
| A / B | X or E / Z |
| Start | Enter |
| Select | Backspace or Right Shift |
| Pause | P |
| Fast-forward (hold) | Tab |
| Save state / load state (not yet for GBA games) | K / L |
| Select state slot | 1-9 |
| Screenshot (and mark the frame) | C |
| GBA L / R shoulders (GBA games only; ignored on a Game Boy) | Q / R |
| Quit | Esc |

## Recording, replay and screenshots

Press **C** to save the exact 160×144 picture as a PNG next to the ROM, named `<rom>-f<frame>.png`. It is greyscale for a DMG, and RGB (RGB555 widened with no colour correction) for a Game Boy Color game in colour mode.

`gb --record session.gbinput game.gb` also records every button change, keyed by frame, plus the battery RAM and the console model (DMG or Game Boy Color, as `--model` resolved it) the session started from. The file grows by about 9 bytes per button change, which is roughly 20–100 KB per hour of play. Screenshots taken while recording mark their frame in the file. The window title shows `[rec]`.

The core is deterministic, so a recording reproduces the session exactly without a window:

```sh
cargo run -p gb-test --release -- --replay session.gbinput game.gb               # dump every marked frame + the last one
cargo run -p gb-test --release -- --replay session.gbinput game.gb --dump 600,900 --out shots/
```

Loading a save state (**L**) while recording ends the recording at that frame, with a note on stderr, because the replay could not follow the jump. The file still replays up to that point; start a new `--record` to go on. Saving a state does not affect a recording.

Replayed frames go to `target/gb-test-out/replay/<recording>/` by default. A replayed marker frame is byte-identical to the screenshot taken while playing. A recording made with a different ROM is rejected, and a Game Boy Color session replays as a Game Boy Color. Recordings made before the model was stored (format version 1) replay as a DMG. If the game crashes or is killed, the file lacks its end record and replays up to the last input.

## Tests

```sh
./scripts/fetch-test-roms.sh                 # Blargg, mooneye, dmg-acid2, cgb-acid2, SameSuite dma, AGE speed-switch, cgb_sound -> roms/test (gitignored)
make -C homebrew                             # needed for the homebrew cases (they fail when CI=true and the ROMs are missing)
./scripts/fetch-homebrew-corpus.sh           # open-source homebrew corpus -> roms/corpus
cargo test --workspace                       # unit tests + one test per ROM
cargo run -p gb-test --release               # pass/fail table
cargo run -p gb-test --release -- --bench 1200 3 --check --min-speed 5   # speed + pinned output hashes
scripts/docker-linux-test.sh [amd64|arm64]   # the Linux CI job in Docker; builds homebrew with Linux GBDK (tools/gbdk-linux/), needs roms/test + roms/corpus
```

The benchmark compares each ROM's video, serial and audio hashes against `crates/gb-test/bench-pins.txt`. That catches any behaviour change, including from a "pure" optimisation. If a homebrew ROM is rebuilt from changed source, re-pin with `--print-pins`. CI also fails if the emulator drops below 5× real time. cgb-acid2 and uCity run on a CGB in colour mode (their video hashes cover the RGB555 frame; uCity switches to double speed), so the "overall" speed that `--min-speed` checks includes the colour renderer and double speed.

`crates/gb-test/expected.txt` is a ratchet. A case fails if its outcome changes in either direction, so a regression or an unrecorded fix both break the build.

cgb-acid2 (`acid2-cgb` in the manifest) runs on a Game Boy Color and compares the RGB555 frame, widened to 8 bits per channel with no colour correction, to its reference image. It passes (DECISIONS #35, #36).

### Test-ROM results

These are the results at the head of `feat/cgb`, run on macOS arm64 on 2026-11-04. Linux x86_64 was last checked with `scripts/docker-linux-test.sh amd64` (fmt, clippy, tests, ROM table) at `0160611` (136/136), before the CGB work. GitHub Actions has not run yet, because this build had no push access.

| Suite | Pass / total |
|-------|--------------|
| Blargg `cpu_instrs` (11 + combined) | 12 / 12 |
| Blargg `instr_timing` | 1 / 1 |
| Blargg `mem_timing` (3 + combined) | 4 / 4 |
| dmg-acid2 | 1 / 1 |
| Mooneye (DMG acceptance subset + MBC1/MBC5) | 87 / 87 |
| Blargg `dmg_sound` (12 + combined) | 13 / 13 |
| Our homebrew: smoke, scripted playability, battery save round-trip, sound effects | 8 / 8 |
| Open-source homebrew corpus (7 games, MBC1/MBC3+RTC/MBC5): smoke + 3 scripted play | 10 / 10 |
| cgb-acid2 (run as a Game Boy Color, RGB555) | 1 / 1 |
| Our CGB homebrew `cgbdemo`: colour test card, scripted play | 1 / 1 |
| Mooneye CGB subset (run as a CGB, DMG-compatibility mode) | 5 / 5 |
| SameSuite `dma` (run as a CGB: GDMA / HDMA) | 4 / 4 |
| AGE `speed-switch` (CGB) | 2 / 6 |
| Blargg `cgb_sound` (CGB, 12 + combined) | 7 / 13 |
| CGB homebrew corpus (six games, run as a CGB, `smoke-cgb`) | 6 / 6 |
| **Total** | **162 / 172** |

The Mooneye CGB cases (`boot_regs-cgb`, `boot_hwio-C`, `unused_hwio-C`, `boot_div-cgbABCDE`, `ppu/vblank_stat_intr-C`) all pass. The ten failing cases are expected failures in `expected.txt`, each with a note: four AGE speed-switch ROMs (DIV-reset edges of the fast timers, LCD / APU phase across a switch; DECISIONS #38), and six `cgb_sound` ROMs (CGB APU differences). Some PPU timing is fitted to mooneye rather than derived from hardware (see DECISIONS #20).

## Cartridges

| Mapper | Status |
|--------|--------|
| ROM only (MBC0) | supported |
| MBC1 (+RAM, battery; MBC1M multicart detection) | supported, passes mooneye `mbc1/*` |
| MBC3 (+RAM, battery, RTC) | supported. The RTC is driven by emulated time, not the host clock, and is saved in the VBA/BGB footer format |
| MBC30 (MBC3 with >2 MiB ROM or 64 KiB RAM) | supported (detected from header); no test ROM |
| MBC5 (+RAM, battery; rumble ignored) | supported, passes mooneye `mbc5/*` |
| MBC2, MMM01, HuC*, camera | not supported (load error) |

## Homebrew games

See `homebrew/README.md`. In short, GBDK-2020 4.5.0 must be in `tools/gbdk/` (gitignored), then:

```sh
make -C homebrew          # -> roms/homebrew/snake.gb, roms/homebrew/breakout.gb, roms/homebrew/cgbdemo.gb
```

- **Snake:** the D-pad steers and Start begins.
- **Breakout:** left/right move the paddle, A or B launches the ball, and Start begins.

Both have title, score and game-over screens. Each keeps a high score in battery RAM (MBC1+RAM+BATTERY), so it persists in `<rom>.sav`, and has sound effects.

- **cgbdemo:** a CGB-only colour test card (header 0x143 = 0xC0, so it needs a Game Boy Color). It shows the eight BG palettes, flipped tiles and a VRAM-bank-1 sprite. The D-pad moves the sprite, and A steps one palette entry. It has a headless play case that checks exact RGB555 pixels.

### Open-source homebrew corpus

`scripts/fetch-homebrew-corpus.sh` downloads 13 redistributable games (7 DMG, 6 CGB), each pinned and sha256-checked. The licences are in the script header. The DMG games:
- Tobu Tobu Girl
- Squishy the Turtle
- Sushi Nights
- Breksta's Cat (MBC3+RTC)
- Adjustris
- AIRPLANZ
- Renegade Rush

The six CGB games are Tobu Tobu Girl Deluxe, Escape Obliterizer DX, Rebound, Aevilia, uCity and Geometrix. They run as a Game Boy Color in `smoke-cgb` cases that check the RGB555 frame (no MBC3 CGB game with a usable licence exists).

They stand in for commercial games in the harness. All 13 boot, and 3 of the DMG games have PyBoy-validated scripted play cases.

## Known gaps

- **PPU.** It renders a whole scanline at the start of mode 3, so mid-scanline register writes (including palette and attribute writes in colour mode) take effect on the next line. Mode-3 sprite penalties are fitted for SCX=0 with no window, and colour mode reuses the DMG mode-3 timing.
- **APU.** It passes all of Blargg `dmg_sound`. Output is a signed [-1, 1] stream: a bipolar DAC plus the DMG high-pass filter, and exactly 0 when the APU or every DAC is off. Zombie mode follows the Pan Docs rules. Still missing:
  - the sub-M-cycle low bits of the square frequency timer on trigger
  - the duty-step-0 quirk after power-on
  - model-specific zombie variants
- **STOP.** Like hardware, it wakes only on a selected joypad line. Unlike hardware, the LCD and timer keep running during STOP.
- **Game Boy Color.** Supported: VRAM and WRAM banking, the KEY1 speed switch and double speed, GDMA / HDMA, the colour PPU (palette RAM, BG attributes, CGB sprite priority, OPRI) with RGB555 output, FF72-FF75 scratch registers and PCM12/PCM34. Still missing:
  - Speed-switch edge cases: the LCD and APU phase across a switch and the DIV-reset edges of the fast timer rates are not exact (AGE `speed-switch` 2 / 6; DECISIONS #38). CGB APU differences fail 6 Blargg `cgb_sound` ROMs.
  - No colourisation of DMG games: a flag-less cartridge on a forced `--model cgb` runs in DMG-compatibility mode in the `--palette` shades, because the boot ROM's compatibility palettes aren't modelled.
  - No infrared port (RP, FF56).
  - CGB-mode post-boot timing is approximate. The DIV and serial phase and the PPU line and dot at hand-off are the compat-mode calibration or a guess, because no test ROM pins them (DECISIONS #33, #34). The CGB boot ROM's clearing of wave RAM isn't modelled either. The palette-port lock timing reuses the VRAM lock timing and is unverified (DECISIONS #36).
- **Not supported:** link cable, debugger, rewind and shaders.
- **No boot ROM.** The emulator starts in the documented post-boot state.

Design decisions are in `DECISIONS.md`, status in `PROGRESS.md`, and deferred review findings in `REVIEW_NOTES.md`.
