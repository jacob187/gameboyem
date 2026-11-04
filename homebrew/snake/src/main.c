/* SNAKE - grid snake on the background layer. DMG, GBDK-2020.
 *
 * Screen (20x18 tiles): row 0 is the HUD, rows 1..17 hold a wall-bordered
 * play field of 18x15 cells (tile columns 1..18, tile rows 2..16).
 */
#include <gb/gb.h>
#include <gbdk/console.h>
#include <rand.h>
#include <stdint.h>

#include "common.h"
#include "sfx.h"

/* ---- tiles ---------------------------------------------------------- */

#define TILE_EMPTY (TILE_BASE + 0u)
#define TILE_BODY  (TILE_BASE + 1u)
#define TILE_HEAD  (TILE_BASE + 2u)
#define TILE_FOOD  (TILE_BASE + 3u)
#define TILE_WALL  (TILE_BASE + 4u)
#define TILE_COUNT 5u

/* 2 bytes per row: low bitplane, high bitplane (colour = hi*2 + lo). */
static const uint8_t tiles[TILE_COUNT * 16u] = {
    /* empty */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    /* body: dark grey block with a gap around it */
    0x00, 0x00, 0x00, 0x7E, 0x00, 0x7E, 0x00, 0x7E,
    0x00, 0x7E, 0x00, 0x7E, 0x00, 0x7E, 0x00, 0x00,
    /* head: black block */
    0x00, 0x00, 0x7E, 0x7E, 0x7E, 0x7E, 0x7E, 0x7E,
    0x7E, 0x7E, 0x7E, 0x7E, 0x7E, 0x7E, 0x00, 0x00,
    /* food: black diamond */
    0x00, 0x00, 0x18, 0x18, 0x3C, 0x3C, 0x7E, 0x7E,
    0x7E, 0x7E, 0x3C, 0x3C, 0x18, 0x18, 0x00, 0x00,
    /* wall: dark/black checker */
    0xAA, 0xFF, 0x55, 0xFF, 0xAA, 0xFF, 0x55, 0xFF,
    0xAA, 0xFF, 0x55, 0xFF, 0xAA, 0xFF, 0x55, 0xFF,
};

/* ---- field geometry ------------------------------------------------- */

#define FIELD_W 18u
#define FIELD_H 15u
#define ORG_X   1u /* tile column of cell (0,0) */
#define ORG_Y   2u /* tile row of cell (0,0) */
#define MAX_LEN (FIELD_W * FIELD_H)

#define RING_SIZE 512u /* power of two >= MAX_LEN */
#define RING_MASK (RING_SIZE - 1u)

enum { CELL_EMPTY = 0, CELL_SNAKE, CELL_FOOD };
enum { DIR_UP = 0, DIR_RIGHT, DIR_DOWN, DIR_LEFT };

static const int8_t dir_dx[4] = { 0, 1, 0, -1 };
static const int8_t dir_dy[4] = { -1, 0, 1, 0 };

/* ---- game state ----------------------------------------------------- */

static uint8_t grid[FIELD_H][FIELD_W];
static uint8_t ring_x[RING_SIZE];
static uint8_t ring_y[RING_SIZE];
static uint16_t head_i; /* index of the head in the ring */
static uint16_t tail_i; /* index of the tail in the ring */
static uint16_t length;
static uint16_t foods;  /* food eaten this game */
static uint16_t score;
static uint16_t best; /* high score, persisted in cartridge RAM */
static uint8_t won;

static void put_cell(uint8_t cx, uint8_t cy, uint8_t tile)
{
    set_bkg_tile_xy(ORG_X + cx, ORG_Y + cy, tile);
}

static void draw_score(void)
{
    print_uint(6, 0, score, 5);
}

/* Frames between snake steps: starts slow, speeds up every 2 foods. */
static uint8_t step_delay(void)
{
    uint16_t level = foods >> 1;

    return (level >= 10u) ? 4u : (uint8_t)(14u - level);
}

static void place_food(void)
{
    uint16_t k = randw() % (MAX_LEN - length);
    uint8_t x, y;

    for (y = 0; y < FIELD_H; y++) {
        for (x = 0; x < FIELD_W; x++) {
            if (grid[y][x] == CELL_EMPTY) {
                if (k == 0u) {
                    grid[y][x] = CELL_FOOD;
                    put_cell(x, y, TILE_FOOD);
                    return;
                }
                k--;
            }
        }
    }
}

static void draw_field(void)
{
    uint8_t i;

    cls();
    fill_bkg_rect(0, 1, 20, 17, TILE_EMPTY);
    for (i = 0; i < 20; i++) {
        set_bkg_tile_xy(i, 1, TILE_WALL);
        set_bkg_tile_xy(i, 17, TILE_WALL);
    }
    for (i = 1; i < 17; i++) {
        set_bkg_tile_xy(0, i, TILE_WALL);
        set_bkg_tile_xy(19, i, TILE_WALL);
    }
    print_at(0, 0, "SCORE");
    print_at(13, 0, "HI");
    print_uint(15, 0, best, 5);
    draw_score();
}

static void new_game(void)
{
    uint8_t x, y;

    for (y = 0; y < FIELD_H; y++) {
        for (x = 0; x < FIELD_W; x++) {
            grid[y][x] = CELL_EMPTY;
        }
    }
    score = 0;
    foods = 0;
    won = 0;
    length = 3;
    tail_i = 0;
    head_i = 2;
    for (x = 0; x < 3; x++) {
        ring_x[x] = 6u + x;
        ring_y[x] = 7u;
        grid[7][6u + x] = CELL_SNAKE;
    }

    DISPLAY_OFF;
    draw_field();
    put_cell(6, 7, TILE_BODY);
    put_cell(7, 7, TILE_BODY);
    put_cell(8, 7, TILE_HEAD);
    place_food();
    DISPLAY_ON;
}

/* Runs one game to completion. Sets `score` and `won`. */
static void play(void)
{
    uint8_t dir = DIR_RIGHT;
    uint8_t next = DIR_RIGHT;
    uint8_t frames = 0;
    uint8_t delay;
    uint8_t keys;
    int8_t nx, ny;
    uint8_t eat;

    new_game();
    delay = step_delay(); /* after new_game(): it resets `foods` */

    for (;;) {
        wait_vbl_done();

        /* Latch the newest turn request; never allow a 180 degree reversal. */
        keys = joypad();
        if ((keys & J_UP) && dir != DIR_DOWN) {
            next = DIR_UP;
        } else if ((keys & J_DOWN) && dir != DIR_UP) {
            next = DIR_DOWN;
        } else if ((keys & J_LEFT) && dir != DIR_RIGHT) {
            next = DIR_LEFT;
        } else if ((keys & J_RIGHT) && dir != DIR_LEFT) {
            next = DIR_RIGHT;
        }

        if (++frames < delay) {
            continue;
        }
        frames = 0;
        dir = next;

        nx = (int8_t)ring_x[head_i & RING_MASK] + dir_dx[dir];
        ny = (int8_t)ring_y[head_i & RING_MASK] + dir_dy[dir];
        if (nx < 0 || ny < 0 || nx >= (int8_t)FIELD_W || ny >= (int8_t)FIELD_H) {
            break; /* wall */
        }

        eat = (grid[ny][nx] == CELL_FOOD);
        if (!eat) {
            /* The tail moves away first, so stepping onto it is legal. */
            uint8_t tx = ring_x[tail_i & RING_MASK];
            uint8_t ty = ring_y[tail_i & RING_MASK];

            grid[ty][tx] = CELL_EMPTY;
            put_cell(tx, ty, TILE_EMPTY);
            tail_i++;
        }
        if (grid[ny][nx] == CELL_SNAKE) {
            break; /* self collision */
        }

        put_cell(ring_x[head_i & RING_MASK], ring_y[head_i & RING_MASK], TILE_BODY);
        head_i++;
        ring_x[head_i & RING_MASK] = (uint8_t)nx;
        ring_y[head_i & RING_MASK] = (uint8_t)ny;
        grid[ny][nx] = CELL_SNAKE;
        put_cell((uint8_t)nx, (uint8_t)ny, TILE_HEAD);

        if (eat) {
            length++;
            foods++;
            score += 10u;
            sfx_eat();
            draw_score();
            delay = step_delay();
            if (length >= MAX_LEN) {
                won = 1;
                break; /* board is full */
            }
            place_food();
        }
    }

    sfx_game_over();
    hiscore_update(&best, score);
    /* Hold the final frame briefly so the collision is visible. */
    for (frames = 0; frames < 45; frames++) {
        wait_vbl_done();
    }
}

static void title_screen(void)
{
    uint8_t i;

    DISPLAY_OFF;
    cls();
    fill_bkg_rect(0, 0, 20, 18, TILE_EMPTY);
    print_at(7, 4, "SNAKE");
    for (i = 4; i < 15; i++) {
        set_bkg_tile_xy(i, 8, TILE_BODY);
    }
    set_bkg_tile_xy(15, 8, TILE_HEAD);
    set_bkg_tile_xy(17, 8, TILE_FOOD);
    print_at(2, 12, "D-PAD: STEER");
    print_at(5, 6, "HI");
    print_uint(8, 6, best, 5);
    DISPLAY_ON;
    wait_start_and_seed(4, 14);
}

static void game_over_screen(void)
{
    DISPLAY_OFF;
    cls();
    fill_bkg_rect(0, 0, 20, 18, TILE_EMPTY);
    print_at(won ? 6 : 5, 4, won ? "YOU WIN" : "GAME OVER");
    print_at(4, 8, "SCORE");
    print_uint(10, 8, score, 5);
    print_at(4, 10, "HI");
    print_uint(10, 10, best, 5);
    DISPLAY_ON;
    wait_start_and_seed(4, 14);
}

void main(void)
{
    hb_init();
    sfx_init();
    best = hiscore_load();
    HIDE_SPRITES;
    SHOW_BKG;
    set_bkg_data(TILE_BASE, TILE_COUNT, tiles);

    title_screen();
    for (;;) {
        play();
        game_over_screen();
    }
}
