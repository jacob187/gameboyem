# gb: a Game Boy (DMG) emulator in Rust

A cycle-accurate-enough original Game Boy emulator, plus two homebrew games written for it.

| Crate | What it is |
|-------|------------|
| `crates/gb-core` | Pure emulation. It has no platform deps, no I/O, no `unsafe`, and runs deterministically. Builds anywhere, including `wasm32`. |
| `crates/gb-frontend` | The `gb` desktop binary (winit + pixels window, cpal audio). This is the only platform-specific crate. |
| `crates/gb-test` | A headless test-ROM harness. Every ROM is a `cargo test` case, and a table runner prints the results. |
| `homebrew/` | Snake and Breakout in C (GBDK-2020). |

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
usage: gb [options] [--] <rom.gb|rom.gbc>
  --scale N        integer window scale, 1..=8 (default 4; HiDPI-aware)
  --mute           disable audio output
  --palette NAME   green (default) or grey (DMG only)
  --model NAME     auto (default: CGB for CGB-flagged ROMs, else DMG), dmg or cgb
  --test-pattern   synthetic pattern + tone instead of running the core
  --               end of options; the next argument is the ROM path
```

Battery RAM is saved to `<rom>.sav` next to the ROM. The save is written atomically every 5 s when RAM changes, on pause, on quit, and on SIGINT/SIGTERM/SIGHUP (Ctrl-C). If writing fails, a warning is printed at once and then at most once a minute, and the failure is always reported on exit.

If an existing `<rom>.sav` can't be read, isn't a regular file, is larger than any valid save (128 KiB + 48 bytes), or is a wrong-size save that couldn't be copied to a backup, it's left untouched. Saving is then disabled for that session, with a warning at startup and "progress NOT saved" on exit. A wrong-size save that can be copied is backed up to `<rom>.sav.bak` (then `.bak1`, ...) before it's replaced. For MBC3 carts with a clock, periodic and pause saves happen only when the game changes cart RAM or an RTC register. The running clock is written on quit.

## Save states

Save states snapshot the whole machine, so you can save and resume anywhere, not just at a game's own save point. Press **S** to save to the current slot and **L** to load it. **1**–**9** pick the slot (default 1). Slot *n* is stored as `<rom>.ss<n>` next to the ROM and written atomically. The window title briefly shows the result, which is also printed to stderr.

A state is tied to the exact ROM it was taken with. A state from another ROM, a damaged file, or one from an incompatible version is rejected, and the running game is left untouched.

Loading a state also rewinds battery RAM, and the next autosave writes that RAM over `<rom>.sav`. So before the first load in a session, the existing `<rom>.sav` is copied to `<rom>.sav.bak` (then `.bak1`, ...). If that copy fails, the load is refused.

## Controls

| Game Boy | Keyboard |
|----------|----------|
| D-pad | Arrow keys |
| A / B | X / Z |
| Start | Enter |
| Select | Backspace or Right Shift |
| Pause | P |
| Fast-forward (hold) | Tab |
| Save state / load state | S / L |
| Select state slot | 1-9 |
| Quit | Esc |

## Tests

```sh
./scripts/fetch-test-roms.sh                 # Blargg, mooneye, dmg-acid2, cgb-acid2 -> roms/test (gitignored)
make -C homebrew                             # needed for the homebrew cases (they fail when CI=true and the ROMs are missing)
./scripts/fetch-homebrew-corpus.sh           # open-source homebrew corpus -> roms/corpus
cargo test --workspace                       # unit tests + one test per ROM
cargo run -p gb-test --release               # pass/fail table
cargo run -p gb-test --release -- --bench 1200 3 --check --min-speed 5   # speed + pinned output hashes
scripts/docker-linux-test.sh [amd64|arm64]   # the Linux CI job in Docker; builds homebrew with Linux GBDK (tools/gbdk-linux/), needs roms/test + roms/corpus
```

The benchmark compares each ROM's video, serial and audio hashes against `crates/gb-test/bench-pins.txt`. That catches any behaviour change, including from a "pure" optimisation. If a homebrew ROM is rebuilt from changed source, re-pin with `--print-pins`. CI also fails if the emulator drops below 5× real time.

`crates/gb-test/expected.txt` is a ratchet. A case fails if its outcome changes in either direction, so a regression or an unrecorded fix both break the build.

cgb-acid2 (`acid2-cgb` in the manifest) runs on a Game Boy Color and compares the RGB555 frame, widened to 8 bits per channel with no colour correction, to its reference image. It is recorded as an expected failure until the CGB PPU lands, so it is not counted in the table below.

### Test-ROM results

These are the results on `integration`, run on macOS arm64. Linux x86_64 was checked with `scripts/docker-linux-test.sh amd64` (fmt, clippy, tests, ROM table) at `0160611` (136/136). GitHub Actions has not run yet, because this build had no push access.

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
| **Total** | **136 / 136** |

Every case in the harness passes. Some PPU timing is fitted to mooneye rather than derived from hardware (see DECISIONS #20).

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
make -C homebrew          # -> roms/homebrew/snake.gb, roms/homebrew/breakout.gb
```

- **Snake:** the D-pad steers and Start begins.
- **Breakout:** left/right move the paddle, A or B launches the ball, and Start begins.

Both have title, score and game-over screens. Each keeps a high score in battery RAM (MBC1+RAM+BATTERY), so it persists in `<rom>.sav`, and has sound effects.

### Open-source homebrew corpus

`scripts/fetch-homebrew-corpus.sh` downloads 7 redistributable DMG games, each pinned and sha256-checked. The licences are in the script header:
- Tobu Tobu Girl
- Squishy the Turtle
- Sushi Nights
- Breksta's Cat (MBC3+RTC)
- Adjustris
- AIRPLANZ
- Renegade Rush

They stand in for commercial games in the harness. All 7 boot, and 3 have PyBoy-validated scripted play cases.

## Known gaps

- **PPU.** It renders a whole scanline at the start of mode 3, so mid-scanline register writes take effect on the next line. Mode-3 sprite penalties are fitted for SCX=0 with no window.
- **APU.** It passes all of Blargg `dmg_sound`. Output is a signed [-1, 1] stream: a bipolar DAC plus the DMG high-pass filter, and exactly 0 when the APU or every DAC is off. Zombie mode follows the Pan Docs rules. Still missing:
  - the sub-M-cycle low bits of the square frequency timer on trigger
  - the duty-step-0 quirk after power-on
  - model-specific zombie variants
- **STOP.** Like hardware, it wakes only on a selected joypad line. Unlike hardware, the LCD and timer keep running during STOP.
- **Not supported:** Game Boy Color, link cable, debugger, rewind and shaders.
- **No boot ROM.** The emulator starts in the documented post-boot state.

Design decisions are in `DECISIONS.md`, status in `PROGRESS.md`, and deferred review findings in `REVIEW_NOTES.md`.
