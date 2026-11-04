# Homebrew games

Two original DMG games written in C with [GBDK-2020](https://github.com/gbdk-2020/gbdk-2020) 4.5.0.
Both are plain 32 KB ROM-only cartridges (cart type 0x00, no MBC, no save RAM).

| Game     | Output        | Controls |
|----------|---------------|----------|
| Snake    | `snake.gb`    | D-pad: steer. Start: begin / restart. |
| Breakout | `breakout.gb` | D-pad left/right: move paddle. A or B: launch ball. Start: begin / restart. |

Snake speeds up as it eats; score and session best are on the HUD.
Breakout has 3 lives, 6 brick rows (higher rows score more), ball angle set by
where it meets the paddle, and a rebuilt, slightly faster board when cleared.

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

- `common/`: text/number printing, palette init, blinking PRESS START wait that seeds the RNG.
- `snake/src/main.c`, `breakout/src/main.c`: one file per game.
- Tiles 0..101 are GBDK's font; game art starts at tile 128.
