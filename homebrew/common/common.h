/* Helpers shared by the homebrew games (DMG, GBDK-2020). */
#ifndef HB_COMMON_H
#define HB_COMMON_H

#include <stdint.h>

/* VRAM tile layout shared by both games. Tiles 0..101 belong to GBDK's
 * built-in font (loaded by font_init()); game art starts at TILE_BASE. */
#define TILE_BASE 128u

/* Standard DMG palette: 0 white, 1 light grey, 2 dark grey, 3 black. */
#define PALETTE_DEFAULT 0xE4u

/* Set up the font/console and the default palettes. Call once at boot. */
void hb_init(void);

/* Print a NUL-terminated string at tile position (x, y). */
void print_at(uint8_t x, uint8_t y, const char *s);

/* Print an unsigned number right-aligned in `width` columns (width <= 5). */
void print_uint(uint8_t x, uint8_t y, uint16_t v, uint8_t width);

/* Show a blinking "PRESS START" at (x, y) and block until Start is pressed,
 * then seed the RNG from DIV and the number of frames waited. */
void wait_start_and_seed(uint8_t x, uint8_t y);

/* ---- Persistent high score (battery-backed cartridge RAM) ----------------
 *
 * Record at the start of SRAM bank 0 (0xA000), 7 bytes:
 *   +0..3  magic "GBHS"
 *   +4     score, low byte        (little endian)
 *   +5     score, high byte
 *   +6     checksum = ((lo * 31 + hi) & 0xFF) ^ 0xA5
 * Fresh, erased or corrupt RAM fails the magic/checksum test and reads as 0.
 * The rest of the 8 KiB is unused. Needs cart type 0x03 (MBC1+RAM+BATTERY).
 */

/* Load the stored high score; 0 when there is no valid record. */
uint16_t hiscore_load(void);

/* If `score` beats `*record`, store it in cartridge RAM and in `*record`.
 * Returns 1 if a new record was written, 0 (and touches no RAM) otherwise. */
uint8_t hiscore_update(uint16_t *record, uint16_t score);

#endif
