/* Stub SDL_mixer.h: upstream i_sound.c does `#include <SDL_mixer.h>` whenever
 * FEATURE_SOUND is defined, but nothing in it is used there — the actual
 * mixing lives in the sound module, and ours (i_wksound.c) mixes in software
 * onto wk:webaudio via ../../audio-compat instead of SDL_mixer. An empty
 * header on the -Icompat include path satisfies the include without touching
 * upstream. */
