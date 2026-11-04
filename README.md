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

**Windows.** `cargo build --release` works with the MSVC toolchain. CI runs it as a non-blocking job.

**Command-line options:**

```
usage: gb [options] [--] <rom.gb>
  --scale N        integer window scale, 1..=8 (default 4; HiDPI-aware)
  --mute           disable audio output
  --palette NAME   green (default) or grey
  --test-pattern   synthetic pattern + tone instead of running the core
  --               end of options; the next argument is the ROM path
```

Battery RAM is saved to `<rom>.sav` next to the ROM. The save is written atomically every 5 s when RAM changes, on pause, on quit, and on SIGINT/SIGTERM/SIGHUP (Ctrl-C). If writing fails, a warning is printed at once and then at most once a minute, and the failure is always reported on exit.

## Controls

| Game Boy | Keyboard |
|----------|----------|
| D-pad | Arrow keys |
| A / B | X / Z |
| Start | Enter |
| Select | Backspace or Right Shift |
| Pause | P |
| Fast-forward (hold) | Tab |
| Quit | Esc |

## Tests

```sh
./scripts/fetch-test-roms.sh                 # Blargg, mooneye, dmg-acid2 -> roms/test (gitignored)
make -C homebrew                             # optional: homebrew smoke/playability cases
cargo test --workspace                       # unit tests + one test per ROM
cargo run -p gb-test --release               # pass/fail table
scripts/docker-linux-test.sh [amd64|arm64]   # the Linux CI job, locally in Docker
```

`crates/gb-test/expected.txt` is a ratchet. A case fails if its outcome changes in either direction, so a regression or an unrecorded fix both break the build.

### Test-ROM results

These are the results on `integration` for macOS arm64 and Linux x86_64 (Docker).

| Suite | Pass / total |
|-------|--------------|
| Blargg `cpu_instrs` (11 + combined) | 12 / 12 |
| Blargg `instr_timing` | 1 / 1 |
| Blargg `mem_timing` (3 + combined) | 4 / 4 |
| dmg-acid2 | 1 / 1 |
| Mooneye (DMG acceptance subset + MBC1/MBC5) | 87 / 87 |
| Blargg `dmg_sound` (12 + combined) | 13 / 13 |
| Homebrew smoke, scripted playability, battery save round-trip | 6 / 6 |
| **Total** | **124 / 124** |

Every case in the harness passes. Some PPU timing is fitted to mooneye rather than derived from hardware (see DECISIONS #20).

## Cartridges

| Mapper | Status |
|--------|--------|
| ROM only (MBC0) | supported |
| MBC1 (+RAM, battery; MBC1M multicart detection) | supported, passes mooneye `mbc1/*` |
| MBC3 (+RAM, battery, RTC) | supported. The RTC is driven by emulated time, not the host clock, and is saved in the VBA/BGB footer format |
| MBC5 (+RAM, battery; rumble ignored) | supported, passes mooneye `mbc5/*` |
| MBC2, MMM01, HuC*, camera | not supported (load error) |

## Homebrew games

See `homebrew/README.md`. In short, GBDK-2020 4.5.0 must be in `tools/gbdk/` (gitignored), then:

```sh
make -C homebrew          # -> roms/homebrew/snake.gb, roms/homebrew/breakout.gb
```

- **Snake:** the D-pad steers and Start begins.
- **Breakout:** left/right move the paddle, A or B launches the ball, and Start begins.

Both have title, score and game-over screens. Each keeps a high score in battery RAM (MBC1+RAM+BATTERY), so it persists in `<rom>.sav`.

## Known gaps

- **PPU.** It renders a whole scanline at the start of mode 3, so mid-scanline register writes take effect on the next line. Mode-3 sprite penalties are fitted for SCX=0 with no window.
- **APU.** It passes all of Blargg `dmg_sound`. Still missing:
  - DMG zombie mode
  - the envelope trigger quirk
  - the high-pass filter. The output is unipolar, so channel starts and stops can click.
- **STOP.** Like hardware, it wakes only on a selected joypad line. Unlike hardware, the LCD and timer keep running during STOP.
- **Not supported:** Game Boy Color, link cable, debugger, rewind and shaders.
- **No boot ROM.** The emulator starts in the documented post-boot state.

Design decisions are in `DECISIONS.md`, status in `PROGRESS.md`, and deferred review findings in `REVIEW_NOTES.md`.
