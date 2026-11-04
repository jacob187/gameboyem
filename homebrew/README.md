# Homebrew games

Two original DMG games written in C with [GBDK-2020](https://github.com/gbdk-2020/gbdk-2020) 4.5.0.
Both are 32 KB cartridges of type 0x03 (MBC1+RAM+BATTERY) with 8 KiB of battery-backed RAM
(header RAM size code 0x02), used to keep the high score.

| Game     | Output        | Controls |
|----------|---------------|----------|
| Snake    | `snake.gb`    | D-pad: steer. Start: begin / restart. |
| Breakout | `breakout.gb` | D-pad left/right: move paddle. A or B: launch ball. Start: begin / restart. |

Snake speeds up as it eats; score and the high score (HI) are on the HUD.
Breakout has 3 lives, 6 brick rows (higher rows score more), ball angle set by
where it meets the paddle, and a rebuilt, slightly faster board when cleared.

## High score and saves

Each game keeps one high score (HI) in cartridge RAM. HI is loaded at boot, shown
on the title and game-over screens (and on snake's HUD), and written when a game
ends with a score above it. Nothing is written otherwise.

Record at `0xA000` (SRAM bank 0), 7 bytes; the rest of the 8 KiB is unused:

| Offset | Content |
|--------|---------|
| 0..3   | magic `"GBHS"` |
| 4..5   | score, little endian |
| 6      | checksum `((lo * 31 + hi) & 0xFF) ^ 0xA5` |

A fresh, erased or corrupt save fails the magic or checksum test and reads as 0.
A write interrupted by power loss leaves a record that fails the checksum (or has
no magic yet), so HI reads 0 rather than a torn value. RAM is enabled only around each
access (`ENABLE_RAM`/`DISABLE_RAM`). The helpers are `hiscore_load()` and
`hiscore_update()` in `common/`.

Emulators keep the RAM in a `.sav` file next to the ROM. Delete it to reset HI.
The headless harness cases `save-snake` / `save-breakout` (`crates/gb-test`) play a
game to game over, load the resulting battery RAM into a fresh machine and check
that its title screen differs from a fresh-RAM one.

## Sounds

Both games have sound effects, written straight to the APU registers by
`common/sfx.c` (`sfx_init()` at boot sets NR52=0x80, NR50=0x77, NR51=0xFF and
silences every channel). Effects are fire and forget: the length counter,
envelope or hardware sweep ends each one, so game code never waits on them.
Only the level-clear arpeggio is stepped, by `sfx_update()` once per frame. The
tones are original and simple; CH3 (wave) is unused.

| Effect | Game | Channel | Sound |
|--------|------|---------|-------|
| Start pressed (title, game over) | both | CH2 | ~885 Hz blip, 78 ms |
| Eat | snake | CH1 | rising sweep ~440 Hz to ~1.8 kHz, about 50 ms |
| Game over | both | CH1 + CH4 | falling sweep with envelope decay (~0.7 s) over a short noise crunch |
| Paddle hit | breakout | CH2 | ~440 Hz blip, 39 ms |
| Brick break | breakout | CH1 | 31 ms blip, one pitch per row (top row C6, bottom row C5) |
| Life lost | breakout | CH4 | 190 ms of low decaying noise |
| Level clear | breakout | CH1 + CH2 | C5 E5 G5 C6 arpeggio, 6 frames a note, CH2 echoing one note behind |

The harness cases `sfx-snake` / `sfx-breakout` (`crates/gb-test`) drain
`audio_samples()` every frame and compare peak-to-peak levels (so they do not
depend on the APU's DC offset or filtering). The title and the game running
must be silent; Start, eat, game over (snake) and Start, brick, paddle, life
lost, game over (breakout) must be loud; and control runs without the input (no
Start press, no steering to the food, no launch) must be quiet over the same
frames. Level clear is not reached by the bot, so it is not in the harness.

## Build

GBDK-2020 4.5.0 must be unpacked so that `$(GBDK_HOME)/bin/lcc` exists.

```sh
make -C homebrew                                  # GBDK_HOME=../tools/gbdk/  OUT=../roms/homebrew
make -C homebrew GBDK_HOME="$PWD/tools/gbdk/"     # explicit (what CI does)
make -C homebrew usage                            # ROM/RAM usage from the linker maps
make -C homebrew clean
```

Intermediate files go to `homebrew/build/` (gitignored); only the `.gb` files
are copied to `$(OUT)`.

## Layout

- `common/`: text/number printing, palette init, blinking PRESS START wait that seeds the RNG, high-score save/load.
- `common/sfx.c`: sound effects (APU registers).
- `snake/src/main.c`, `breakout/src/main.c`: one file per game.
- Tiles 0..101 are GBDK's font; game art starts at tile 128.
