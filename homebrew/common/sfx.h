/* Sound effects for the homebrew games (DMG APU, registers written directly).
 *
 * Every effect is fire and forget: it programs a channel and returns. Length
 * counters, envelopes and the hardware sweep end the sound, so callers need no
 * timing. The one exception is the level-clear arpeggio, which is stepped by
 * sfx_update() once per frame.
 *
 * Channel use: CH1 (sweep) eat, brick, game over, arpeggio; CH2 Start, paddle,
 * arpeggio echo; CH4 (noise) life lost, game over. CH3 is not used.
 */
#ifndef HB_SFX_H
#define HB_SFX_H

#include <stdint.h>

/* Power the APU (NR52=0x80, NR50=0x77, NR51=0xFF) and silence every channel. */
void sfx_init(void);

/* Step the multi-note effects; call once per frame from a game loop. */
void sfx_update(void);

void sfx_start(void);      /* Start pressed: CH2 blip */
void sfx_eat(void);        /* snake eats: CH1 rising sweep blip */
void sfx_game_over(void);  /* CH1 falling sweep + CH4 noise burst */
void sfx_paddle(void);     /* ball meets paddle: CH2 */
void sfx_brick(uint8_t row); /* brick broken, row 0 = top (highest pitch): CH1 */
void sfx_life_lost(void);  /* CH4 noise */
void sfx_level_clear(void); /* CH1/CH2 arpeggio, needs sfx_update() */

#endif
