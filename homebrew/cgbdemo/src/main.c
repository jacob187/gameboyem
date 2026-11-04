/* CGBDEMO - a colour test card. CGB-only (header 0x143 = 0xC0), GBDK-2020.
 *
 * Background (20x18 tiles, attributes in VRAM bank 1):
 *   rows 1..3   8 swatch columns, palette 0..7, showing colours 1..3 of each
 *   row 6       one asymmetric tile: plain, X-flipped, Y-flipped, both (palette
 *               1), then plain again in palettes 3, 4 and 5
 * A sprite (OBJ palette 1, tile data in VRAM bank 1) is moved with the D-pad.
 * A steps the last swatch (BG palette 7, colour 3) through CYCLE[] by writing
 * BCPS (auto-increment) and two BCPD bytes.
 */
#include <gb/cgb.h>
#include <gb/gb.h>
#include <stdint.h>

/* ---- tiles ---------------------------------------------------------- */

#define TILE_EMPTY 0u
#define TILE_SOLID 1u /* +0, +1, +2: solid colour 1, 2, 3 */
#define TILE_QUAD  4u
#define BKG_COUNT  5u

#define SPR_TILE   8u /* bank 1 holds the sprite; bank 0 holds a decoy */

/* 2 bytes per row: low bitplane, high bitplane (colour = hi*2 + lo). */
static const uint8_t bkg_tiles[BKG_COUNT * 16u] = {
    /* empty */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    /* solid colour 1 */
    0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00,
    0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00,
    /* solid colour 2 */
    0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF,
    0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF,
    /* solid colour 3 */
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    /* quad: four 4x4 quadrants, top left 1, top right 2, bottom left 3,
     * bottom right 0, so every flip puts a different colour in each corner */
    0xF0, 0x0F, 0xF0, 0x0F, 0xF0, 0x0F, 0xF0, 0x0F,
    0xF0, 0xF0, 0xF0, 0xF0, 0xF0, 0xF0, 0xF0, 0xF0,
};

/* Sprite: a colour 3 core with a colour 1 rim. */
static const uint8_t spr_tile[16] = {
    0xFF, 0x00, 0xFF, 0x7E, 0xFF, 0x7E, 0xFF, 0x7E,
    0xFF, 0x7E, 0xFF, 0x7E, 0xFF, 0x7E, 0xFF, 0x00,
};
/* Bank 0 decoy for the same tile number: solid colour 1. Drawn only if the
 * sprite ignored its bank bit. */
static const uint8_t spr_decoy[16] = {
    0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00,
    0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00,
};

/* ---- palettes (RGB555, 4 colours each) -------------------------------- */

#define SWATCHES 8u
#define CYCLED_PAL   7u /* BG palette 7, colour 3: stepped by A */
#define CYCLED_COLOR 3u

static const palette_color_t bkg_pal[SWATCHES * 4u] = {
    /* 0: blue, colour 0 is the backdrop */
    RGB(2, 2, 6), RGB(6, 8, 16), RGB(10, 14, 24), RGB(14, 20, 31),
    /* 1: the quad tile's own colours */
    RGB(0, 0, 0), RGB(31, 6, 6), RGB(6, 28, 6), RGB(8, 8, 31),
    /* 2: red */
    RGB(8, 0, 0), RGB(14, 2, 2), RGB(22, 4, 4), RGB(31, 8, 8),
    /* 3: orange */
    RGB(8, 4, 0), RGB(16, 10, 0), RGB(24, 16, 0), RGB(31, 22, 2),
    /* 4: yellow */
    RGB(8, 8, 0), RGB(15, 15, 0), RGB(23, 23, 2), RGB(31, 31, 6),
    /* 5: green */
    RGB(0, 8, 0), RGB(2, 14, 2), RGB(4, 22, 4), RGB(8, 31, 8),
    /* 6: teal */
    RGB(0, 8, 8), RGB(0, 14, 14), RGB(2, 22, 22), RGB(6, 31, 31),
    /* 7: magenta; colour 3 is CYCLE[0] */
    RGB(8, 0, 8), RGB(14, 2, 14), RGB(22, 4, 22), RGB(31, 8, 31),
};

static const palette_color_t obj_pal[4] = {
    RGB(0, 0, 0), RGB(28, 28, 31), RGB(0, 0, 0), RGB(31, 12, 24),
};

static const palette_color_t CYCLE[] = {
    RGB(31, 8, 31), RGB(22, 31, 4), RGB(10, 22, 31), RGB(31, 14, 10),
    RGB(31, 31, 31),
};
#define CYCLE_LEN (sizeof CYCLE / sizeof CYCLE[0])

/* ---- sprite ---------------------------------------------------------- */

#define SPR_START_X 80
#define SPR_START_Y 96
#define SPR_MIN_X   8
#define SPR_MAX_X   152
#define SPR_MIN_Y   64 /* stay below the tile rows */
#define SPR_MAX_Y   136
#define SPR_SPEED   2

/* ---- screen ---------------------------------------------------------- */

/* Tile number in VRAM bank 0 and attribute byte in bank 1 (bits 0-2 palette,
 * bit 5 X flip, bit 6 Y flip) for one map cell. */
static void put(uint8_t x, uint8_t y, uint8_t tile, uint8_t attr)
{
    VBK_REG = VBK_TILES;
    set_bkg_tile_xy(x, y, tile);
    VBK_REG = VBK_ATTRIBUTES;
    set_bkg_tile_xy(x, y, attr);
    VBK_REG = VBK_TILES;
}

static void draw_card(void)
{
    uint8_t p, row, x, y;

    for (y = 0; y < 18u; y++) {
        for (x = 0; x < 20u; x++) {
            put(x, y, TILE_EMPTY, 0);
        }
    }
    for (p = 0; p < SWATCHES; p++) {
        for (row = 0; row < 3u; row++) {
            put(2u + 2u * p, 1u + row, TILE_SOLID + row, p);
            put(3u + 2u * p, 1u + row, TILE_SOLID + row, p);
        }
    }
    put(2, 6, TILE_QUAD, 1);
    put(5, 6, TILE_QUAD, 1 | S_FLIPX);
    put(8, 6, TILE_QUAD, 1 | S_FLIPY);
    put(11, 6, TILE_QUAD, 1 | S_FLIPX | S_FLIPY);
    put(14, 6, TILE_QUAD, 3);
    put(16, 6, TILE_QUAD, 4);
    put(18, 6, TILE_QUAD, 5);
}

/* Set BG palette CYCLED_PAL, colour CYCLED_COLOR. Call in VBlank: the palette
 * data port is locked while the PPU is drawing. The index auto-increments, so
 * the high byte needs no second BCPS write. */
static void set_cycled(palette_color_t c)
{
    BCPS_REG = BCPSF_AUTOINC | (CYCLED_PAL * 8u + CYCLED_COLOR * 2u);
    BCPD_REG = (uint8_t)(c & 0xFFu);
    BCPD_REG = (uint8_t)(c >> 8);
}

void main(void)
{
    uint8_t keys, prev = 0, step = 0;
    uint8_t x = SPR_START_X, y = SPR_START_Y;

    DISPLAY_OFF;
    set_bkg_palette(0, SWATCHES, bkg_pal);
    set_sprite_palette(1, 1, obj_pal);
    set_bkg_data(TILE_EMPTY, BKG_COUNT, bkg_tiles);
    set_sprite_data(SPR_TILE, 1, spr_decoy);
    VBK_REG = VBK_ATTRIBUTES; /* the same VRAM, bank 1 */
    set_sprite_data(SPR_TILE, 1, spr_tile);
    VBK_REG = VBK_TILES;
    draw_card();
    set_sprite_tile(0, SPR_TILE);
    set_sprite_prop(0, S_BANK | S_PAL(1));
    move_sprite(0, x + 8u, y + 16u);
    SHOW_BKG;
    SHOW_SPRITES;
    DISPLAY_ON;

    for (;;) {
        wait_vbl_done();
        keys = joypad();
        if ((keys & J_A) && !(prev & J_A)) {
            step = (step + 1u == CYCLE_LEN) ? 0u : step + 1u;
            set_cycled(CYCLE[step]);
        }
        prev = keys;
        if ((keys & J_LEFT) && x > SPR_MIN_X) {
            x -= SPR_SPEED;
        }
        if ((keys & J_RIGHT) && x < SPR_MAX_X) {
            x += SPR_SPEED;
        }
        if ((keys & J_UP) && y > SPR_MIN_Y) {
            y -= SPR_SPEED;
        }
        if ((keys & J_DOWN) && y < SPR_MAX_Y) {
            y += SPR_SPEED;
        }
        move_sprite(0, x + 8u, y + 16u);
    }
}
