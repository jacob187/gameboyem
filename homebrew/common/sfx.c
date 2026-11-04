#include <gb/gb.h>

#include "sfx.h"

/* Frequency register value x gives 131072 / (2048 - x) Hz. */
#define NOTE_C5 0x705u
#define NOTE_E5 0x739u
#define NOTE_G5 0x759u
#define NOTE_C6 0x783u

#define TRIGGER  0x80u
#define LEN_EN   0x40u

/* NRx1: duty 50% (bits 7-6 = 2) | length load; the sound lasts (64 - load)/256 s. */
#define DUTY50(load) ((uint8_t)(0x80u | (load)))

static void ch1(uint8_t sweep, uint8_t duty_len, uint8_t env, uint16_t freq, uint8_t len_en)
{
    NR10_REG = sweep;
    NR11_REG = duty_len;
    NR12_REG = env;
    NR13_REG = (uint8_t)freq;
    NR14_REG = (uint8_t)(TRIGGER | len_en | (freq >> 8));
}

static void ch2(uint8_t duty_len, uint8_t env, uint16_t freq)
{
    NR21_REG = duty_len;
    NR22_REG = env;
    NR23_REG = (uint8_t)freq;
    NR24_REG = (uint8_t)(TRIGGER | LEN_EN | (freq >> 8));
}

static void noise(uint8_t load, uint8_t env, uint8_t clock)
{
    NR41_REG = load;
    NR42_REG = env;
    NR43_REG = clock;
    NR44_REG = TRIGGER | LEN_EN;
}

/* ---- arpeggio ---------------------------------------------------------- */

#define ARP_NOTES   4u
#define ARP_STEPS   (ARP_NOTES + 1u) /* the extra step lets CH2 echo the last note */
#define ARP_FRAMES  6u
#define ARP_LEAD_IN 4u               /* frames to let the brick blip be heard */

static const uint16_t arp_note[ARP_NOTES] = { NOTE_C5, NOTE_E5, NOTE_G5, NOTE_C6 };
static uint8_t arp_step = ARP_STEPS; /* ARP_STEPS = idle */
static uint8_t arp_wait;

void sfx_init(void)
{
    NR52_REG = 0x80u; /* APU on first: the other registers ignore writes while off */
    NR50_REG = 0x77u;
    NR51_REG = 0xFFu;
    NR12_REG = 0u; /* DAC off: nothing left sounding from the boot ROM */
    NR22_REG = 0u;
    NR42_REG = 0u;
    arp_step = ARP_STEPS;
}

void sfx_update(void)
{
    if (arp_step >= ARP_STEPS) {
        return;
    }
    if (arp_wait) {
        arp_wait--;
        return;
    }
    if (arp_step < ARP_NOTES) {
        ch1(0u, DUTY50(0x30u), 0xB2u, arp_note[arp_step], LEN_EN);
    }
    if (arp_step > 0u) {
        ch2(DUTY50(0x28u), 0x72u, arp_note[arp_step - 1u]);
    }
    arp_step++;
    arp_wait = ARP_FRAMES - 1u;
}

void sfx_level_clear(void)
{
    arp_step = 0u;
    arp_wait = ARP_LEAD_IN;
}

/* ---- one-shot effects -------------------------------------------------- */

void sfx_start(void)
{
    ch2(DUTY50(0x2Cu), 0xA1u, 0x76Cu); /* ~885 Hz, 78 ms */
}

void sfx_eat(void)
{
    /* Sweep up every 2 steps (15.6 ms), shift 4: ~440 Hz -> ~1.8 kHz, then the
     * sweep overflows and stops the channel (about 50 ms). */
    ch1(0x24u, DUTY50(0x26u), 0xC1u, 0x6D6u, LEN_EN);
}

void sfx_paddle(void)
{
    ch2(DUTY50(0x36u), 0xB1u, 0x6D6u); /* ~440 Hz, 39 ms */
}

void sfx_brick(uint8_t row)
{
    static const uint16_t row_note[6] = { 0x783u, 0x76Bu, 0x759u, 0x739u, 0x721u, 0x705u };

    ch1(0u, DUTY50(0x38u), 0xC1u, row_note[row < 6u ? row : 5u], LEN_EN);
}

void sfx_life_lost(void)
{
    noise(0x0Fu, 0xF2u, 0x65u); /* 190 ms of low noise, decaying */
}

void sfx_game_over(void)
{
    arp_step = ARP_STEPS; /* drop a pending arpeggio */
    /* Falling sweep (~560 Hz down), no length counter: the envelope decays it
     * to silence in about 0.7 s. */
    ch1(0x4Au, DUTY50(0u), 0xF3u, 0x708u, 0u);
    noise(0x2Au, 0xD1u, 0x55u); /* short crunch on top */
}
