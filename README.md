# gb: a Game Boy, Game Boy Color and Game Boy Advance emulator in Rust

A cycle-accurate-enough original Game Boy (DMG) emulator with Game Boy Color (CGB) support, and a Game Boy Advance (GBA) emulator built on the same frontend. One binary, `gb`, runs all three. It comes with homebrew written for it: two DMG games, a CGB colour test card and two GBA games.

- **Game Boy and Game Boy Color:** CPU, PPU, APU, timers, MBC1/3/5, CGB colour, double speed and HDMA.
- **Game Boy Advance:** ARM7TDMI (ARM and Thumb), all PPU modes with affine layers, windows, blending and mosaic, the four PSG channels and two Direct Sound FIFOs, DMA and timers, SRAM / Flash / EEPROM saves, the S-3511 cartridge clock, and a built-in high-level BIOS. No BIOS dump is needed.
- **Frontend:** window and audio, save states, input recording with headless replay, and PNG screenshots, for every system.

| Crate | What it is |
|-------|------------|
| `crates/emu-common` | What every core shares: the `Core` trait the frontend drives, the save-state primitives and the input-recording codec. It has no dependencies. |
| `crates/gb-core` | Pure emulation of the DMG and CGB. It has no platform deps, no I/O, no `unsafe`, and runs deterministically. Builds anywhere, including `wasm32`. |
| `crates/gba-core` | Pure emulation of the Game Boy Advance, with the same rules as `gb-core`: no platform deps, no I/O, deterministic, `wasm32`-clean. Includes the HLE BIOS. |
| `crates/gb-frontend` | The `gb` desktop binary (winit + pixels window, cpal audio). This is the only platform-specific crate. |
| `crates/gb-test` | A headless test-ROM harness for all three systems. Every ROM is a `cargo test` case, and a table runner prints the results. |
| `homebrew/` | Snake and Breakout (DMG) and `cgbdemo` (a CGB-only colour test card) in C (GBDK-2020), and GRAINS and MOTH (GBA) in Rust on `agb`. |

## Download

Builds for macOS, Windows and Linux are on the [Releases page](https://github.com/jacob187/gameboyem/releases). You don't need Rust. Each release has a `SHA256SUMS` file.

- **macOS:** open the `.dmg` for your Mac (`macos-arm64` for Apple silicon, `macos-x86_64` for Intel) and drag gameboyem to Applications.
- **Windows:** run `gameboyem-<version>-windows-x64-setup.exe` (a per-user install, no administrator rights). Or unzip the `-portable.zip` and run `gb.exe`.
- **Linux:** `chmod +x` the `.AppImage` and run it, or `sudo apt install ./gameboyem-<version>-linux-x86_64.deb`. The `.tar.gz` holds the bare `gb` binary. Needs glibc 2.35 or newer (Ubuntu 22.04, Debian 12, Fedora 36 or newer).

Open a ROM with the dialog that appears when you start the app, or drag a `.gb`, `.gbc` or `.gba` file onto its window. Dragging works on macOS, Windows and X11, not under native Wayland. On Linux the dialog needs a desktop portal (`xdg-desktop-portal` with a GTK or KDE backend) or `zenity`. The Windows installer and the `.deb` also associate `.gb`, `.gbc` and `.gba` files with the app, so opening one starts the ROM. The AppImage, the `.tar.gz` and the portable zip register nothing. On macOS the app is listed under "Open With" but doesn't receive the file yet, so it may just start and ask you to choose a ROM (Finder may also say it can't open that format).

**The builds are unsigned**, so the first launch is blocked by the OS:

- **macOS:** Gatekeeper blocks the first open. Right-click the app, choose Open, then Open again. Or run `xattr -dr com.apple.quarantine /Applications/gameboyem.app`.
- **Windows:** SmartScreen shows "Windows protected your PC". Click More info, then Run anyway.

### How to make a release

Set `version` under `[workspace.package]` in `Cargo.toml` to the release number, commit it, then push a tag with a leading `v` that matches it:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

The Release workflow builds every package and publishes them with `SHA256SUMS` on the Releases page. To try it first, open Actions, pick Release and Run workflow: it builds the same packages and keeps them as workflow artifacts without creating a release. The workflow has optional, off-by-default steps for signing and notarizing once Apple and Windows certificates are added as repository secrets (see the comments in `.github/workflows/release.yml`).

## Build and run

You need Rust stable. `gb-frontend` needs ≥ 1.92; `emu-common`, `gb-core`, `gba-core` and `gb-test` need ≥ 1.85. The GBA homebrew is the one exception, because it needs a pinned nightly (see Homebrew games).

**macOS (arm64/x86_64).** No extra system packages are needed.

```sh
cargo build --release
./target/release/gb path/to/game.gb      # or game.gbc, or game.gba
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
usage: gb [options] [--] [rom.gb|rom.gbc|rom.gba]

With no ROM path, a file dialog asks for one. A ROM file can also be dropped onto the window.

options:
  --scale N          integer window scale in logical points, 1..=8 (default 4)
  --mute             disable audio output
  --system NAME      gb or gba (default: a .gba file, or a ROM with a GBA header, is a GBA game)
  --palette NAME     green (default) or grey (Game Boy only; colour games use their own colours)
  --model NAME       auto (default: Game Boy Color for CGB-flagged ROMs, else DMG), dmg or cgb (Game Boy only)
  --bios FILE        boot through a real 16 KiB GBA BIOS dump (GBA only; default: built-in HLE BIOS)
  --save-type NAME   auto (default: from the ROM), none, sram, flash64, flash128, eeprom512 or eeprom8k (GBA only)
  --rtc NAME         auto (default: on when the game sets up the cartridge clock), on or off (GBA only)
  --rtc-start TIME   start the GBA cartridge clock at YYYY-MM-DDTHH:MM:SS if the save has none (GBA only; default 2000-01-01, power-loss flag set)
  --test-pattern     show a synthetic test pattern and tone instead of running the core
  --record FILE      record every input to FILE (must not exist) for `gb-test --replay`
  -h, --help         show this help
  --                 end of options; the next argument is the ROM path, even if it starts with `-`
```

A `.gba` file runs as a GBA game. Any other file is a GBA game if its header is a GBA one (0x96 at 0xB2 and a valid checksum at 0xBD), otherwise a Game Boy game; `--system gb|gba` overrides this. `--model` and `--palette` are ignored with a warning for a GBA game, `--save-type`, `--rtc` and `--rtc-start` are ignored with a warning for a Game Boy game, and `--bios` is an error for a Game Boy game. ROMs are capped at 8 MiB for a Game Boy and 32 MiB for a GBA. For Game Boy, `.gb` and `.gbc` ROMs are both accepted (the header decides, not the extension). With `--model auto`, a ROM whose header byte 0x143 is 0x80 or 0xC0 runs as a Game Boy Color, in colour. Any other ROM runs as a DMG. `--model cgb` forces a Game Boy Color, and a ROM without a CGB flag then runs in DMG-compatibility mode. It isn't colourised, because there's no boot ROM to pick a compatibility palette, so it uses `--palette`. `--model dmg` runs everything on a DMG, including CGB ROMs.

**GBA BIOS.** A GBA game runs on a built-in high-level BIOS by default. The BIOS calls (`SWI`) are implemented in Rust, the interrupt vector and reset code are a small image we wrote ourselves, and the cartridge starts in the state the real BIOS leaves it in, skipping the boot logo. You don't need a BIOS dump and the emulator never asks for one. `--bios FILE` boots through your own 16 KiB dump instead, from address 0, and the BIOS then runs as it does on hardware. Nintendo's BIOS is not included in this repository, and nothing here downloads it. The dump's hash is not checked, so a wrong 16 KiB file boots and misbehaves like any bad BIOS. Save states and recordings made with a dump only load or replay with the same dump (they store its hash, never the dump).

Battery RAM is saved to `<rom>.sav` next to the ROM. The save is written atomically every 5 s when RAM changes, on pause, on quit, and on SIGINT/SIGTERM/SIGHUP (Ctrl-C). If writing fails, a warning is printed at once and then at most once a minute, and the failure is always reported on exit.

If an existing `<rom>.sav` can't be read, isn't a regular file, is larger than any valid save (128 KiB + 48 bytes), or is a wrong-size save that couldn't be copied to a backup, it's left untouched. Saving is then disabled for that session, with a warning at startup and "progress NOT saved" on exit. A wrong-size save that can be copied is backed up to `<rom>.sav.bak` (then `.bak1`, ...) before it's replaced. For MBC3 carts with a clock, periodic and pause saves happen only when the game changes cart RAM or an RTC register. The running clock is written on quit.

**GBA saves.** The save chip comes from the ROM: the first library ID string it contains (`SRAM_V`, `FLASH_V`, `FLASH512_V`, `FLASH1M_V` or `EEPROM_V`) picks it, and a ROM with none has no save. `--save-type` overrides that. The `<rom>.sav` file is the chip's bytes:

| Chip | `.sav` size |
|------|-------------|
| SRAM | 32 KiB |
| Flash 64K / Flash 128K | 64 KiB / 128 KiB |
| EEPROM 512 B / EEPROM 8K | 512 bytes / 8 KiB |

EEPROM carts don't say which size they are. The size is fixed by the first DMA transfer to the chip, or by the size of an existing `.sav`. Until then there is nothing to save, so no file is written. If a game guesses wrong, `--save-type eeprom512` or `eeprom8k` fixes it. A `.sav` whose size doesn't fit the chip is refused with the core's error and backed up to `<rom>.sav.bak`.

A game with a cartridge clock adds a 24-byte footer to the chip's bytes. The footer begins with `AGBRTC01` and holds the clock's registers and the cycles into the current second. It is written once the game has used the clock, so a game without a clock keeps a plain chip-sized `.sav`. This is our own footer, not the 48-byte Game Boy one, and another emulator's clock footer isn't understood. The clock never reads the host's time. Without a footer it starts at 2000-01-01 with the power-loss flag set, so the game asks you for the time, unless you pass `--rtc-start "$(date +%Y-%m-%dT%H:%M:%S)"`. A footer in the save wins over `--rtc-start`. `--rtc auto` leaves the clock port inert until the game's first store that looks like clock setup. `--rtc on` has it from power-on and `--rtc off` removes it, keeping any footer in the save as it was.

## Save states

Save states snapshot the whole machine, so you can save and resume anywhere, not just at a game's own save point. Press **K** to save to the current slot and **L** to load it. **Moved from S:** in earlier versions save state was **S**. S is now D-pad Down (with the arrow keys and W / A / S / D), so on a Game Boy an S press goes to the game as a Down press, and under `--record` it is recorded as one, instead of saving a state. The startup notice and `gb --help` both say K saves. Load is still **L** and the slots are still **1**-**9**. **1**–**9** pick the slot (default 1). Slot *n* is stored as `<rom>.ss<n>` next to the ROM and written atomically. The window title briefly shows the result, which is also printed to stderr. GBA games work the same way (a state is 0.55–0.68 MB). A GBA state is tied to the ROM and to how the session was started: it only loads with the same `--bios` dump (or none) and the same `--save-type` and `--rtc` options, and otherwise the load is refused with a message saying so and nothing changes.

A state is tied to the exact ROM and the console model (DMG or Game Boy Color) it was taken with. A state from another ROM or model, a damaged file, or one from an incompatible version is rejected, and the running game is left untouched. States from before CGB support (format version 1) are DMG states and still load on a DMG. Because `--model auto` now starts CGB-flagged ROMs (header 0x143 = 0x80 or 0xC0) as a Game Boy Color, such a ROM refuses an old state with a message pointing to `--model dmg`; start it with that flag to load the state.

Loading a state also rewinds battery RAM, and the next autosave writes that RAM over `<rom>.sav`. So before the first load in a session, the existing `<rom>.sav` is copied to `<rom>.sav.bak` (then `.bak1`, ...). If that copy fails, the load is refused.

## Controls

| Button (Game Boy and GBA) | Keyboard |
|----------|----------|
| D-pad | Arrow keys or W / A / S / D |
| A / B | X or E / Z |
| Start | Enter |
| Select | Backspace or Right Shift |
| Pause | P |
| Fast-forward (hold) | Tab |
| Save state / load state | K / L (save state was **S** before the GBA work; S is now D-pad Down) |
| Select state slot | 1-9 |
| Screenshot (and mark the frame) | C |
| L / R shoulders (GBA games only; ignored on a Game Boy) | Q / R |
| Quit | Esc |

## Recording, replay and screenshots

Press **C** to save the exact 160×144 picture (240×160 for a GBA game) as a PNG next to the ROM, named `<rom>-f<frame>.png`. It is greyscale for a DMG, and RGB (RGB555 widened with no colour correction) for a Game Boy Color game in colour mode and for every GBA game.

`gb --record session.gbinput game.gb` also records every button change, keyed by frame, plus the battery RAM and the console model (DMG or Game Boy Color, as `--model` resolved it) the session started from. The file grows by about 9 bytes per button change, which is roughly 20–100 KB per hour of play. Screenshots taken while recording mark their frame in the file. The window title shows `[rec]`.

A GBA recording also stores what powers the machine on the way the session did: `--save-type`, `--rtc`, `--rtc-start` and, for `--bios`, the dump's hash (never the dump). `gb-test --replay` reads it back; a recording made with a real BIOS replays only with the same dump, `--replay session.gbinput game.gba --bios bios.bin`, and says so if it is missing or different.

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
./scripts/fetch-gba-test-roms.sh             # jsmolka gba-tests, FuzzARM, tonc demos -> roms/test/gba (add --with-sst for the ~20 MB-a-file SingleStepTests CPU data)
make -C homebrew                             # needed for the homebrew cases (they fail when CI=true and the ROMs are missing)
make -C homebrew gba                         # the two GBA games; needs the pinned nightly (see Homebrew games)
./scripts/fetch-homebrew-corpus.sh           # open-source homebrew corpus -> roms/corpus
cargo test --workspace                       # unit tests + one test per ROM
cargo run -p gb-test --release               # pass/fail table
cargo run -p gb-test --release -- --bench 1200 3 --check --min-speed 5   # speed + pinned output hashes
scripts/docker-linux-test.sh [amd64|arm64]   # the Linux CI job in Docker; builds homebrew with Linux GBDK (tools/gbdk-linux/), needs roms/test + roms/corpus
```

The benchmark runs six Game Boy ROMs and four GBA ROMs (FuzzARM `ARM_Any`, `moth`, `grains` and tonc `m7_demo`) and compares each ROM's video, serial and audio hashes against `crates/gb-test/bench-pins.txt`. That catches any behaviour change, including from a "pure" optimisation. If a homebrew ROM is rebuilt from changed source, re-pin with `--print-pins`. CI also fails if the emulator drops below 5× real time, checked separately for each system against its own overall rate. On a dev Mac the Game Boy ROMs run at about 40× real time and the GBA ROMs at about 17–24× depending on machine load, so the floor is at most about a third of the GBA figure. cgb-acid2 and uCity run on a CGB in colour mode (their video hashes cover the RGB555 frame; uCity switches to double speed), so the "overall" speed that `--min-speed` checks includes the colour renderer and double speed.

`crates/gb-test/expected.txt` is a ratchet. A case fails if its outcome changes in either direction, so a regression or an unrecorded fix both break the build.

cgb-acid2 (`acid2-cgb` in the manifest) runs on a Game Boy Color and compares the RGB555 frame, widened to 8 bits per channel with no colour correction, to its reference image. It passes.

### Test-ROM results

These are the results at the head of `feat/gba` (`bd6ce54`), from `cargo run -p gb-test --release` on macOS arm64 on 2026-11-06. Linux x86_64 was last checked with `scripts/docker-linux-test.sh amd64` (fmt, clippy, tests, ROM table) at `0160611` (136/136), before the CGB and GBA work. GitHub Actions has not run yet, because this build had no push access.

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
| GBA jsmolka `gba-tests`: `arm`, `thumb`, `memory`, `nes` (CPU and memory; the ROM's failed-test register must be 0) | 4 / 4 |
| GBA jsmolka `save`: `sram`, `flash64`, `flash128`, `none` | 4 / 4 |
| GBA FuzzARM (`ARM_Any`, `ARM_DataProcessing`, `THUMB_Any`, `THUMB_DataProcessing`, `FuzzARM`; 10,000 random instruction tests each, EWRAM result word must stay 0) | 5 / 5 |
| GBA visual ROMs (jsmolka `ppu/*`, 12 tonc demos): smoke only, the frame must not be one flat colour | 15 / 15 |
| Our GBA homebrew `grains` and `moth`: smoke | 2 / 2 |
| **Total** | **192 / 202** |

The GBA rows are 30 of the 202. Four things about them. First, the visual ROMs only prove that the PPU draws something, not that the picture is right (`gb-test --dump-gba ROM FRAMES OUT_DIR` writes PNGs for looking at them). Second, SingleStepTests/ARM7TDMI, which checks the CPU per instruction down to the bus transactions, runs under `cargo test -p gba-core` and isn't in this table. It needs `--with-sst` (about 900 MB) and is skipped without the files, in CI as well as locally, because CI never fetches the set (see Known gaps). Third, jsmolka `bios.gba` (the official BIOS's open-bus values) runs only when `GBA_BIOS` points at a real dump, so it is skipped in CI, and `unsafe.gba` fails on real hardware and is left out. Last, the mGBA test suite isn't run (see Known gaps).

The Mooneye CGB cases (`boot_regs-cgb`, `boot_hwio-C`, `unused_hwio-C`, `boot_div-cgbABCDE`, `ppu/vblank_stat_intr-C`) all pass. The ten failing cases are expected failures in `expected.txt`, each with a note: four AGE speed-switch ROMs (DIV-reset edges of the fast timers, LCD / APU phase across a switch), and six `cgb_sound` ROMs (CGB APU differences). Some PPU timing is fitted to mooneye rather than derived from hardware: the STAT mode bits lag the internal mode by one M-cycle, LCD-on line 0 starts at dot 4, the summed sprite penalty is cut by 3 dots, and the sprite fit was only checked with SCX = 0 and no window.

## Cartridges

| Mapper | Status |
|--------|--------|
| ROM only (MBC0) | supported |
| MBC1 (+RAM, battery; MBC1M multicart detection) | supported, passes mooneye `mbc1/*` |
| MBC3 (+RAM, battery, RTC) | supported. The RTC is driven by emulated time, not the host clock, and is saved in the VBA/BGB footer format |
| MBC30 (MBC3 with >2 MiB ROM or 64 KiB RAM) | supported (detected from header); no test ROM |
| MBC5 (+RAM, battery; rumble ignored) | supported, passes mooneye `mbc5/*` |
| MBC2, MMM01, HuC*, camera | not supported (load error) |

A GBA cartridge has no mapper. The ROM is up to 32 MiB, and the save chip is chosen as described under Build and run:

| Backup chip | Status |
|-------------|--------|
| SRAM (32 KiB) | supported, passes jsmolka `save/sram` |
| Flash 64K / 128K | supported (unlock sequence, ID mode, byte program, sector and chip erase, 128K bank switching); passes jsmolka `save/flash64` and `flash128`. Erase and program finish instantly. The chip IDs are Panasonic (64K) and Sanyo (128K), and no commercial game was checked against them |
| EEPROM 512 B / 8 KiB | supported, with the size taken from the first DMA transfer, a loaded save or `--save-type` |
| none | supported, passes jsmolka `save/none` |
| S-3511 real-time clock (GPIO port) | supported, driven by emulated time, with no alarm or per-minute interrupt (see Known gaps) |
| Solar sensor, rumble, gyro (the other users of the GPIO port) | not supported |

## Homebrew games

See `homebrew/README.md`. In short, GBDK-2020 4.5.0 must be in `tools/gbdk/` (gitignored), then:

```sh
make -C homebrew          # -> roms/homebrew/snake.gb, roms/homebrew/breakout.gb, roms/homebrew/cgbdemo.gb
```

- **Snake:** the D-pad steers and Start begins.
- **Breakout:** left/right move the paddle, A or B launches the ball, and Start begins.

Both have title, score and game-over screens. Each keeps a high score in battery RAM (MBC1+RAM+BATTERY), so it persists in `<rom>.sav`, and has sound effects.

- **cgbdemo:** a CGB-only colour test card (header 0x143 = 0xC0, so it needs a Game Boy Color). It shows the eight BG palettes, flipped tiles and a VRAM-bank-1 sprite. The D-pad moves the sprite, and A steps one palette entry. It has a headless play case that checks exact RGB555 pixels.

### GBA homebrew

Two original GBA games in Rust on [`agb`](https://github.com/agbrs/agb) 0.25.0, with all art and sound generated in code. They are in `homebrew/gba/`; see `homebrew/gba/README.md` for the controls, smoke-test scripts and ROM hashes.

- **GRAINS:** a falling-sand toy in raw mode 4 with page flipping. The D-pad moves the brush, A pours, B erases, L / R pick the material (sand, water, stone), Select toggles the faucet and Start clears.
- **MOTH:** a night-sky collector in mode 0 with four backgrounds, animated sprites and a software mixer playing through Direct Sound DMA. The D-pad flies, A flies faster and Start begins a round.

They are a separate cargo project with its own pinned nightly (`nightly-2026-07-22`, needed for `-Zbuild-std` on `thumbv4t-none-eabi`) so the main workspace stays on stable Rust. Build them with:

```sh
rustup toolchain install nightly-2026-07-22 --component rust-src,clippy,rustfmt --profile minimal
cargo install agb-gbafix --version 0.25.0 --locked --root tools/agb   # or any directory on PATH
make -C homebrew gba                                                  # -> roms/homebrew/grains.gba, roms/homebrew/moth.gba
```

`make -C homebrew` with no target still builds only the GBDK ROMs. Both ROMs are byte-reproducible with that toolchain.

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

### Game Boy and Game Boy Color

- **PPU.** It renders a whole scanline at the start of mode 3, so mid-scanline register writes (including palette and attribute writes in colour mode) take effect on the next line. Mode-3 sprite penalties are fitted for SCX=0 with no window, and colour mode reuses the DMG mode-3 timing.
- **APU.** It passes all of Blargg `dmg_sound`. Output is a signed [-1, 1] stream: a bipolar DAC plus the DMG high-pass filter, and exactly 0 when the APU or every DAC is off. Zombie mode follows the Pan Docs rules. Still missing:
  - the sub-M-cycle low bits of the square frequency timer on trigger
  - the duty-step-0 quirk after power-on
  - model-specific zombie variants
- **STOP.** Like hardware, it wakes only on a selected joypad line. Unlike hardware, the LCD and timer keep running during STOP.
- **Game Boy Color.** Supported: VRAM and WRAM banking, the KEY1 speed switch and double speed, GDMA / HDMA, the colour PPU (palette RAM, BG attributes, CGB sprite priority, OPRI) with RGB555 output, FF72-FF75 scratch registers and PCM12/PCM34. Still missing:
  - Speed-switch edge cases: the LCD and APU phase across a switch and the DIV-reset edges of the fast timer rates are not exact (AGE `speed-switch` 2 / 6). The switch pause is 32768 M-cycles with the timer running, which is what AGE `spsw-tima` and `spsw-div` pin, but the immediate TIMA increments the DIV reset causes at 262 / 65 / 16 kHz land one M-cycle off. CGB APU differences fail 6 Blargg `cgb_sound` ROMs.
  - No colourisation of DMG games: a flag-less cartridge on a forced `--model cgb` runs in DMG-compatibility mode in the `--palette` shades, because the boot ROM's compatibility palettes aren't modelled.
  - No infrared port (RP, FF56).
  - CGB-mode post-boot timing is approximate. The DIV and serial phase and the PPU line and dot at hand-off are the compat-mode calibration or a guess, because no test ROM pins them. The CGB boot ROM's clearing of wave RAM isn't modelled either. The palette-port lock timing reuses the VRAM lock timing and is unverified.
- **No boot ROM.** The emulator starts in the documented post-boot state.

### Game Boy Advance

Everything here is a deliberate approximation or an unimplemented feature.

- **Commercial games are untested by CI.** No commercial ROM is committed or fetched, so the emulator is checked only against test ROMs and our own homebrew. Whether a given game runs well is for you to find out. Open-bus values, odd DMA corners, and games that depend on exact timing are where problems are most likely to show.
- **The mGBA test suite isn't run.** It has no prebuilt ROM and needs devkitARM to build, which means Docker, and Docker isn't used by default in this project. The visual test ROMs are smoke checks only, and `armwrestler` and the nba-emu `hw-test` ROMs aren't wired up.
- **CI doesn't run the CPU SingleStepTests.** The ARM7TDMI SingleStepTests set is about 900 MB and only `./scripts/fetch-gba-test-roms.sh --with-sst` fetches it, which CI doesn't run. So all 45 SST tests skip and pass on every CI leg without checking anything. CI still runs jsmolka `arm`, `thumb` and `memory` and FuzzARM, but a CPU bug only SST would catch (the N/S kind of a load/store-multiple bus transaction, say) shows up only when you run `cargo test -p gba-core` with the set fetched. A populated set with a missing file does fail under `CI`.
- **HLE BIOS.**
  - The sound-driver calls (`SoundDriverInit`, `Mode`, `Main`, `VSync`, `VSyncOff`, `VSyncOn`, `SoundChannelClear`, the undocumented 0x20-0x24 and `SoundGetJumpList`) do nothing and return. Most commercial games link their own driver, but one that calls the BIOS's gets no sound from it.
  - `ArcTan`, `ArcTan2` and the sine table behind `BgAffineSet` and `ObjAffineSet` are written from the documented algorithm, not from a real BIOS, and nothing checks them against one. Only tan(1.0) = 0x2000 is pinned. `BgAffineSet` / `ObjAffineSet` rounding may differ by one LSB, and `MidiKey2Freq` and some `RegisterRamReset` resets are approximated.
  - The cycle cost of each call is a plausible magnitude, not a measurement. `MultiBoot` always reports failure. A memory-moving call (`CpuSet`, decompression, affine sets) stops at a 16-frame cycle budget, which protects the host from hostile arguments and does not match hardware.
  - The real BIOS comparison needs a dump: the `GBA_BIOS` test cases are skipped in CI, and `--bios` is the way to avoid all of the above.
- **Bus timing.**
  - DMA runs exclusively of the CPU, so CPU and DMA bus contention isn't modelled. The one-cycle palette / VRAM / OAM access penalty while the PPU is reading, the 1210-cycle per-line sprite budget and the DISPCNT enable-bit write delay aren't modelled either.
  - Components are caught up after each instruction, so a timer overflow, DMA start or interrupt can be late by up to one instruction. The 1 to 2 cycle offset between a status flag and its IF bit isn't modelled.
  - The prefetch buffer follows GBATEK, but its behaviour on a branch or a data access while a halfword is in flight (no wait, and a data access keeps the buffer) is unverified against hardware. A pure-ROM, bus-bound loop costs exactly the classic wait-state timing.
  - Open bus returns the last fetched opcode, not the real pipeline-dependent value. IRQ entry is charged like an SWI, and the exact figure isn't known.
- **Timers fast enough to storm are slow.** A timer is only scheduled when something observes it, so unobserved ones cost nothing. A timer with its interrupt enabled that overflows every few cycles costs an event each time: ten emulated frames of that took 0.47 s in a release build on a dev Mac, well below real time. That slows such a game and never the host.
- **Cartridge clock.** The alarm and the per-minute interrupt are not implemented. Alarm 1, alarm 2 and the free register read 0xFF and drop what is written to them, and control bit 3 (the per-minute interrupt) is stored and does nothing. The sizes of registers 5 and 7 are a guess. The `--rtc auto` rule is a heuristic: a non-RTC game that stores a small value at the clock port wakes it, which is harmless. Real time never enters the core unless you pass `--rtc-start`.
- **Saves.** Flash erase and program are instant. A game with no library ID string gets no save unless you pass `--save-type`. A driver that bit-bangs the EEPROM with the CPU instead of DMA can get the wrong size guess (GBATEK says it doesn't work on hardware); `--save-type` fixes it.
- **PPU.**
  - Each line is drawn at once at cycle 960. Mid-line effects finer than a line, and a register write between dot 240 and the HBlank flag at cycle 1006 landing on the next line, are the price. A DISPSTAT or LYC write that makes the VCount match true mid-line raises no interrupt, because neither GBATEK nor the hw-test says what hardware does.
  - Windows and blending follow GBATEK with two deviations or guesses: a window whose right (bottom) edge is below its left (top) edge wraps, as Tonc and mGBA draw it, and the window's colour-effect bit also switches off a semi-transparent sprite's blend, which is undocumented either way.
  - OBJ mosaic is an approximation: it quantises the sprite's screen position instead of reproducing the hardware's per-line counter, so block alignment can differ.
- **APU.** SOUNDBIAS's resolution bits (6 to 9-bit, 262 to 32 kHz) are stored and read back but not modelled, so the output isn't quantised. A FIFO pop can be applied one instruction late after a register access synced the APU past it. A full FIFO drops the newest bytes, which hardware doesn't document.
- **Serial and link port.** Not emulated. The registers exist and save in states, but nothing connects to them, so link cable, multiplayer, Wireless Adapter and multiboot don't work.
- **Frontend.** The picture is RGB555 widened to 8 bits per channel with no LCD colour correction. The GBA window and audio haven't been checked by the build, only by headless tests: the visual check is pending the user, as on the Game Boy.

### Both

- **Not supported:** debugger, rewind and shaders, and for the Game Boy, the link cable.
