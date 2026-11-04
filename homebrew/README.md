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
- `snake/src/main.c`, `breakout/src/main.c`: one file per game.
- Tiles 0..101 are GBDK's font; game art starts at tile 128.
