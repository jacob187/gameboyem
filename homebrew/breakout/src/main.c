/* BREAKOUT - paddle, ball and bricks. DMG, GBDK-2020.
 *
 * Bricks live on the background layer (tile rows 2..7, full width); the
 * paddle (3 sprites) and ball (1 sprite) are sprites. Ball position and
 * velocity are fixed point in 1/8 pixel units.
 */
#include <gb/gb.h>
#include <gbdk/console.h>
#include <rand.h>
#include <stdint.h>

#include "common.h"

/* ---- tiles ---------------------------------------------------------- */

#define TILE_EMPTY  (TILE_BASE + 0u)
#define TILE_BRICK0 (TILE_BASE + 1u) /* 3 brick styles: +0, +1, +2 */
#define TILE_LIFE   (TILE_BASE + 4u)
#define BKG_COUNT   5u

#define SPR_BASE    (TILE_BASE + 16u)
#define SPR_CAP     (SPR_BASE + 0u)
#define SPR_MID     (SPR_BASE + 1u)
#define SPR_BALL    (SPR_BASE + 2u)
#define SPR_COUNT   3u

/* 2 bytes per row: low bitplane, high bitplane (colour = hi*2 + lo). */
static const uint8_t bkg_tiles[BKG_COUNT * 16u] = {
    /* empty */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    /* brick style 0: black with dark highlight */
    0x00, 0xFE, 0xFE, 0xFE, 0xFE, 0xFE, 0xFE, 0xFE,
    0xFE, 0xFE, 0xFE, 0xFE, 0xFE, 0xFE, 0x00, 0x00,
    /* brick style 1: dark grey with light highlight */
    0xFE, 0x00, 0x00, 0xFE, 0x00, 0xFE, 0x00, 0xFE,
    0x00, 0xFE, 0x00, 0xFE, 0x00, 0xFE, 0x00, 0x00,
    /* brick style 2: light grey with dark bottom edge */
    0xFE, 0x00, 0xFE, 0x00, 0xFE, 0x00, 0xFE, 0x00,
    0xFE, 0x00, 0xFE, 0x00, 0x00, 0xFE, 0x00, 0x00,
    /* life icon: small ball */
    0x00, 0x00, 0x3C, 0x3C, 0x7E, 0x7E, 0x7E, 0x7E,
    0x7E, 0x7E, 0x7E, 0x7E, 0x3C, 0x3C, 0x00, 0x00,
};

static const uint8_t spr_tiles[SPR_COUNT * 16u] = {
    /* paddle left cap (right cap is this tile flipped): 4 px tall */
    0x7F, 0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F, 0x7F,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    /* paddle middle */
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    /* ball: 4x4 block centred in the tile (pixels 2..5) */
    0x00, 0x00, 0x00, 0x00, 0x3C, 0x3C, 0x3C, 0x3C,
    0x3C, 0x3C, 0x3C, 0x3C, 0x00, 0x00, 0x00, 0x00,
};

/* ---- geometry (screen pixels unless noted) --------------------------- */

#define SCREEN_W   160
#define SCREEN_H   144
#define CEILING_Y  8   /* below the HUD row */
#define BR_ROW0    2   /* first brick tile row */
#define BR_ROWS    6
#define BR_COLS    20
#define PAD_Y      136
#define PAD_W      24
#define PAD_H      4
#define PAD_SPEED  3
#define BALL_SIZE  4
#define START_LIVES 3

/* Ball position/velocity are in 1/8 pixel. */
#define FP_SHIFT   3
#define BX_MAX     ((SCREEN_W - BALL_SIZE) << FP_SHIFT)

/* Paddle bounce: 8 zones across the paddle, left to right. */
static const int8_t zone_vx[8] = { -14, -10, -6, -2, 2, 6, 10, 14 };
static const int8_t zone_vy[8] = { 6, 9, 12, 14, 14, 12, 9, 6 }; /* magnitude */

/* ---- game state ----------------------------------------------------- */

static uint8_t bricks[BR_ROWS][BR_COLS];
static uint8_t bricks_left;
static uint8_t pad_x;
static int16_t bx, by; /* ball top-left, 1/8 px */
static int8_t vx, vy;  /* 1/8 px per frame */
static uint8_t serving; /* ball is resting on the paddle */
static uint8_t lives;
static uint8_t level;
static uint16_t score;

static uint8_t speed_bonus(void)
{
    return (level > 4u) ? 3u : (uint8_t)(level - 1u);
}

static void draw_lives(void)
{
    uint8_t i;

    for (i = 0; i < START_LIVES; i++) {
        set_bkg_tile_xy(17u + i, 0, (i < lives) ? TILE_LIFE : TILE_EMPTY);
    }
}

static void draw_hud(void)
{
    print_uint(6, 0, score, 5);
    print_uint(14, 0, level, 2);
    draw_lives();
}

static void build_bricks(void)
{
    uint8_t r, c;

    for (r = 0; r < BR_ROWS; r++) {
        for (c = 0; c < BR_COLS; c++) {
            bricks[r][c] = 1;
            set_bkg_tile_xy(c, BR_ROW0 + r, TILE_BRICK0 + (r >> 1));
        }
    }
    bricks_left = BR_ROWS * BR_COLS;
}

static void draw_sprites(void)
{
    uint8_t sy = (uint8_t)(PAD_Y + 16);

    move_sprite(0, (uint8_t)(pad_x + 8u), sy);
    move_sprite(1, (uint8_t)(pad_x + 16u), sy);
    move_sprite(2, (uint8_t)(pad_x + 24u), sy);
    /* Ball hitbox is 4x4 at (bx, by); its art sits at pixels 2..5 of the tile. */
    move_sprite(3, (uint8_t)((bx >> FP_SHIFT) + 8 - 2), (uint8_t)((by >> FP_SHIFT) + 16 - 2));
}

static void reset_ball(void)
{
    serving = 1;
    bx = (int16_t)(pad_x + (PAD_W / 2) - (BALL_SIZE / 2)) << FP_SHIFT;
    by = (int16_t)(PAD_Y - BALL_SIZE) << FP_SHIFT;
    vx = 0;
    vy = 0;
}

static void launch_ball(void)
{
    serving = 0;
    vx = (rand() & 1u) ? 6 : -6;
    vy = -(int8_t)(12u + speed_bonus());
}

/* Remove every brick overlapping the 4x4 box at pixel (px, py). */
static uint8_t hit_bricks(int16_t px, int16_t py)
{
    uint8_t hit = 0;
    uint8_t i, row, col;
    int16_t x, y;

    for (i = 0; i < 4u; i++) {
        x = px + ((i & 1u) ? (BALL_SIZE - 1) : 0);
        y = py + ((i & 2u) ? (BALL_SIZE - 1) : 0);
        if (x < 0 || y < 0) {
            continue;
        }
        col = (uint8_t)(x >> 3);
        row = (uint8_t)(y >> 3);
        if (col >= BR_COLS || row < BR_ROW0 || row >= BR_ROW0 + BR_ROWS) {
            continue;
        }
        row -= BR_ROW0;
        if (bricks[row][col]) {
            bricks[row][col] = 0;
            bricks_left--;
            set_bkg_tile_xy(col, row + BR_ROW0, TILE_EMPTY);
            score += (uint16_t)(BR_ROWS - row); /* upper rows are worth more */
            hit = 1;
        }
    }
    return hit;
}

/* Advance the ball one frame. Returns 1 if it fell off the bottom. */
static uint8_t step_ball(void)
{
    int16_t px, py;
    uint8_t rel, zone;

    /* Horizontal move: side walls, then bricks. */
    bx += vx;
    if (bx < 0) {
        bx = 0;
        if (vx < 0) vx = -vx;
    } else if (bx > BX_MAX) {
        bx = BX_MAX;
        if (vx > 0) vx = -vx;
    }
    if (hit_bricks(bx >> FP_SHIFT, by >> FP_SHIFT)) {
        bx -= vx;
        vx = -vx;
    }

    /* Vertical move: ceiling, bricks, paddle. */
    by += vy;
    if (by < (CEILING_Y << FP_SHIFT)) {
        by = CEILING_Y << FP_SHIFT;
        if (vy < 0) vy = -vy;
    }
    px = bx >> FP_SHIFT;
    py = by >> FP_SHIFT;
    if (hit_bricks(px, py)) {
        by -= vy;
        vy = -vy;
        py = by >> FP_SHIFT;
    }

    if (vy > 0 && py + BALL_SIZE >= PAD_Y && py < PAD_Y + PAD_H
        && px + BALL_SIZE > (int16_t)pad_x && px < (int16_t)pad_x + PAD_W) {
        /* Angle depends on where the ball centre meets the paddle. */
        px = px + (BALL_SIZE / 2) - (int16_t)pad_x;
        rel = (px < 0) ? 0u : (px >= PAD_W) ? (PAD_W - 1u) : (uint8_t)px;
        zone = rel / 3u;
        vx = zone_vx[zone];
        vy = -(int8_t)(zone_vy[zone] + speed_bonus());
        by = (int16_t)(PAD_Y - BALL_SIZE) << FP_SHIFT;
    }

    return (by >> FP_SHIFT) >= SCREEN_H;
}

static void draw_playfield(void)
{
    DISPLAY_OFF;
    cls();
    fill_bkg_rect(0, 0, 20, 18, TILE_EMPTY);
    print_at(0, 0, "SCORE");
    print_at(12, 0, "LV");
    build_bricks();
    draw_hud();
    DISPLAY_ON;
}

/* Runs one game to completion. */
static void play(void)
{
    uint8_t keys;
    uint16_t shown_score = 0;

    score = 0;
    lives = START_LIVES;
    level = 1;
    pad_x = (SCREEN_W - PAD_W) / 2;

    draw_playfield();
    reset_ball();
    draw_sprites();
    SHOW_SPRITES;

    for (;;) {
        wait_vbl_done();
        keys = joypad();

        if (keys & J_LEFT) {
            pad_x = (pad_x > PAD_SPEED) ? (uint8_t)(pad_x - PAD_SPEED) : 0u;
        }
        if (keys & J_RIGHT) {
            pad_x += PAD_SPEED;
            if (pad_x > SCREEN_W - PAD_W) {
                pad_x = (uint8_t)(SCREEN_W - PAD_W);
            }
        }

        if (serving) {
            bx = (int16_t)(pad_x + (PAD_W / 2) - (BALL_SIZE / 2)) << FP_SHIFT;
            if (keys & (J_A | J_B)) {
                launch_ball();
            }
        } else if (step_ball()) {
            lives--;
            draw_lives();
            if (lives == 0u) {
                draw_hud();
                break;
            }
            reset_ball();
        }

        if (bricks_left == 0u) {
            /* Board cleared: rebuild it, slightly faster, ball back on paddle. */
            level++;
            DISPLAY_OFF;
            build_bricks();
            draw_hud();
            DISPLAY_ON;
            reset_ball();
        }
        if (score != shown_score) {
            shown_score = score;
            print_uint(6, 0, score, 5);
        }
        draw_sprites();
    }

    HIDE_SPRITES;
    /* Hold the final frame briefly. */
    for (keys = 0; keys < 45u; keys++) {
        wait_vbl_done();
    }
}

static void title_screen(void)
{
    uint8_t r;

    DISPLAY_OFF;
    cls();
    fill_bkg_rect(0, 0, 20, 18, TILE_EMPTY);
    print_at(6, 3, "BREAKOUT");
    for (r = 0; r < 3u; r++) {
        fill_bkg_rect(0, 6u + r, 20, 1, TILE_BRICK0 + r);
    }
    print_at(2, 10, "A: LAUNCH BALL");
    print_at(2, 12, "D-PAD: MOVE");
    DISPLAY_ON;
    wait_start_and_seed(4, 15);
}

static void game_over_screen(void)
{
    DISPLAY_OFF;
    cls();
    fill_bkg_rect(0, 0, 20, 18, TILE_EMPTY);
    print_at(5, 4, "GAME OVER");
    print_at(4, 8, "SCORE");
    print_uint(10, 8, score, 5);
    print_at(4, 10, "LEVEL");
    print_uint(10, 10, level, 5);
    DISPLAY_ON;
    wait_start_and_seed(4, 14);
}

void main(void)
{
    hb_init();
    HIDE_SPRITES;
    SHOW_BKG;
    set_bkg_data(TILE_BASE, BKG_COUNT, bkg_tiles);
    set_sprite_data(SPR_BASE, SPR_COUNT, spr_tiles);
    set_sprite_tile(0, SPR_CAP);
    set_sprite_tile(1, SPR_MID);
    set_sprite_tile(2, SPR_CAP);
    set_sprite_prop(2, S_FLIPX);
    set_sprite_tile(3, SPR_BALL);

    title_screen();
    for (;;) {
        play();
        game_over_screen();
    }
}
