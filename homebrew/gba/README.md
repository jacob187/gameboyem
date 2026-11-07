# GBA homebrew

Two small original Game Boy Advance games in Rust on [`agb`](https://github.com/agbrs/agb) 0.25.0. Each one
stands on its own, uses different hardware, and is meant to double as an emulator smoke test. All art and sound is
generated in code; the code is MIT. (`agb` itself is MPL-2.0.)

| Game | ROM | Hardware |
|------|-----|----------|
| GRAINS, a falling-sand toy | `grains.gba` | Raw mode 4 (240x160, 8-bit paletted), page flipping with incremental redraw, palette cycling, VBlank wait, the per-cell loops in IWRAM; D-pad, A, B, L, R, START, SELECT |
| MOTH, a night-sky collector | `moth.gba` | Mode 0, four backgrounds (two scroll), hardware sprites with animation, VBlank interrupt, software mixer (direct sound DMA, timers 0 and 1) |

Both are deterministic from the button input: fixed-seed xorshift RNGs, no hardware RNG, no SRAM. GRAINS uses no timers; MOTH's mixer runs timer 0 as the sample clock for the sound DMA and timer 1 in cascade mode with an overflow IRQ that swaps the sound buffers.

## Build

Needs rustup. The nightly is pinned in `rust-toolchain.toml`; install it once:

```sh
rustup toolchain install nightly-2026-07-22 --component rust-src,clippy,rustfmt --profile minimal
cargo install agb-gbafix --version 0.25.0 --locked --root tools/agb   # or any dir on PATH
make -C homebrew gba                                                  # -> roms/homebrew/{grains,moth}.gba
```

`make -C homebrew` (no target) builds only the GBDK Game Boy ROMs. `make -C homebrew gba-clean` removes the GBA
build output. Cargo must run inside this directory (that is where rustup and `.cargo/config.toml` are found), so
for manual work: `cd homebrew/gba && cargo build --release --locked`, `cargo clippy --release`, `cargo fmt --check`.
`check-header.sh` validates ROM headers and is run by the Makefile target.

Sizes and sha256 with the pinned toolchain (the same on any machine and any `CARGO_HOME`, because `trim-paths` strips the checkout and `~/.cargo` paths from the panic locations):

| ROM | Bytes | sha256 |
|-----|-------|--------|
| `grains.gba` | 18436 | `55885fb566e06e9fcd719ef0a7902ba3bae392dc3f852cebd82379293039daa1` |
| `moth.gba` | 51112 | `2220d181fddb05ff4d133bd8b66f7e3ca897746e07f02d95a8d7dff3a1d905bf` |

`agb` is built with `default-features = false, features = ["testing"]`: no backtrace/QR panic screen. A panic
prints to the mGBA debug log (if present) and halts the CPU, which is also what a headless test wants.

## GRAINS

| Button | Effect |
|--------|--------|
| D-pad | Move the 3x3-cell brush one cell on a press; after 8 frames held it repeats every other frame |
| A | Pour the selected material into empty cells under the brush (every frame while held) |
| B | Erase the cells under the brush |
| R / L | Next / previous material: sand, water, stone (wraps) |
| SELECT | Faucet on / off |
| START | Title: begin. Play: clear the grid |

The grid is 60x36 cells of 4x4 pixels (pixels 0..239 x 0..143); the HUD is the bottom 16 rows. Page flipping:
bit 4 of DISPCNT (0x04000000) alternates every frame in play. A faucet drops one sand grain per frame (until 1200
cells are occupied), sweeping across columns 8..51.

Smoke test (RGB555 values; hold every button at least 2 frames; wait counts are minimums, agb boot takes a few frames):

1. Wait 10 frames. Title: DISPCNT = 0x0404, page 0. Backdrop (any pixel outside text) is 0x1842. The rainbow bar
   (x 56..183, y 80..87) changes colour every 4 frames (palette rotation), and the "PRESS START" text blinks every 30
   frames. No input for 610 frames moves to play on its own (title timeout); otherwise press START.
2. Play (3 frames after START): the HUD strip (y 144..159) is 0x2CA5 where there is no text. The frame counter text
   (x 130..169, y 147..156) differs on every frame, so every frame differs from the previous one. After 60 frames
   there are sand pixels (tan, around 0x26BA) in y < 144 below the faucet.
3. Cursor: the brush outline is a 12x12 box, initially x 116..127, y 36..47, white/yellow (0x7FFF / 0x135F, alternates
   every 8 frames). Tap RIGHT: x 120..131. Tap DOWN: y 40..51. Tap LEFT or UP to go back.
4. Material: the HUD swatch (x 4..11, y 148..155) starts sand 0x26BA. Tap R: water (a blue that shimmers every
   frame); tap R again: stone 0x41CE; tap L: back to water. The label text (x 16..) changes with it.
5. A: hold A for 30 frames with the box at its start: sand fills the box and falls below it; pixels in
   x 116..127, y 48..143 become non-backdrop. B held for 5 frames over sand clears the cells in the box.
6. SELECT toggles the label at x 186.. between FAUCET and OFF, and the faucet stops (no new grains from the top).
   START in play empties the grid (every pixel with y < 144 is backdrop except the cursor box).
7. L and R are the only way to change the material, so step 4 exercises both shoulder buttons.

## MOTH

| Button | Effect |
|--------|--------|
| D-pad | Fly the moth, 2 px per frame |
| A (held) | Fly faster, 3 px per frame |
| START | Title: begin. After "TIME UP": play another round |

Gather the drifting glints (an 8x8 star sprite): 5 are on screen at once, each is worth 1 point, the HUD shows
SCORE (3 digits) and TIME (seconds left). A round lasts 1800 frames. The world scrolls faster as the score rises.
The moth is OAM sprite 0 (16x16), the glints follow. Layers: sky with stars and a moon (slow), hills (1 px per frame
at score 0), HUD, and the title / "TIME UP" overlay.

Sound (mixer at 10512 Hz, direct sound DMA): a rising jingle (3600 samples) on every START that begins a round, a
blip (1800 samples) on each collected glint, a falling tone (4000 samples) when time runs out. Nothing plays on the
title.

Smoke test (hold buttons at least 2 frames; wait counts are minimums):

1. Wait 10 frames. Title: DISPCNT mode 0 with the BG and OBJ enable bits set, four backgrounds. Backdrop 0x2041. The
   hill layer's horizontal scroll register increases by 1 every frame and the sky layer's by 1 every 3 frames, so the
   picture changes every frame. "PRESS START" blinks every 30 frames (0x177F and 0x28A6). No audio yet. No input for
   610 frames starts play by itself; otherwise press START.
2. START: the HUD reads SCORE 000 and TIME 30, the overlay is gone (three backgrounds), and the start jingle plays
   for about 0.34 s (audio output must become non-silent on both channels).
3. Movement: the moth starts at (40, 72). Hold RIGHT for 20 frames: OAM sprite 0 x is 80 (100 if A is held). Hold
   DOWN for 10 frames: y is 92. The wing frame alternates every 6 frames.
4. Collecting, with no input at all after START: glint 0 starts at (277, 79) and moves left 1 px per frame, so it
   touches the moth at about 225 frames after START (plus or minus 3). Then SCORE shows 001 (the HUD digits at
   x 56..79, y 0..15 change), a blip plays, and the world speed rises by 1/16 px per frame. Glint positions depend
   only on the frames since START, so the score at a given frame is fixed by the input.
5. Time up: 1801 frames after START the overlay shows "TIME UP", the falling tone plays, TIME reads 00, scrolling
   slows down and the glints freeze. START then starts a new round (SCORE and TIME reset, jingle plays again).
