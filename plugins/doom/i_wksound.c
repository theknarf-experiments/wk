/* i_wksound.c — DOOM sound effects on wk:webaudio, via ../audio-compat.
 *
 * The second non-upstream source (after doomgeneric_wk.c): upstream i_sound.c
 * dispatches every I_*Sound call through a `sound_module_t DG_sound_module`
 * that i_sdlsound.c (not compiled here) normally defines over SDL_mixer. This
 * file defines that module over a small software mixer instead: DMX sound
 * lumps are decoded to mono f32 on first use, up to 8 channels (s_sound's
 * snd_channels) are mixed with per-channel volume + stereo separation into
 * interleaved stereo at snd_samplerate (44100), and Update() — called every
 * 35 Hz tic via S_UpdateSounds → I_UpdateSound — keeps ~100 ms queued on the
 * wkaudio pcm-queue. The host schedules chunks gaplessly and adds its own
 * ~30 ms lead only when the queue has drained.
 *
 * DMX lump format and the volume/separation math follow i_sdlsound.c
 * (chocolate-doom lineage) so Freedoom's stock sounds behave exactly as they
 * would under the SDL port. Music is out of scope (no OPL emulation):
 * DG_music_module below is a no-op that satisfies i_sound.c's dispatch.
 */
#include <stdlib.h>
#include <string.h>

#include "deh_str.h"
#include "doomtype.h"
#include "i_sound.h"
#include "m_misc.h"
#include "w_wad.h"
#include "z_zone.h"

#include "wkaudio.h"

/* i_sound.c's I_BindSoundVariables references these (they live in
 * i_sdlsound.c upstream); the config machinery wants addresses to bind. */
int use_libsamplerate = 0;
float libsamplerate_scale = 0.65f;

/* Mixer geometry: doom's s_sound.c drives at most snd_channels (8) logical
 * channels; we mix them into stereo chunks of MIX_CHUNK_FRAMES frames and
 * keep TARGET_BUFFERED_SECONDS queued — topped up every 35 Hz tic (28.6 ms),
 * so 100 ms rides comfortably without drift. */
#define NUM_CHANNELS 8
#define MIX_CHUNK_FRAMES 512
#define TARGET_BUFFERED_SECONDS 0.100

/* A DMX lump decoded to mono f32 at its native rate, cached on
 * sfxinfo->driver_data the first time the sound is started. Sounds stay
 * cached for the run (Freedoom 1's whole SFX set is a few MB as f32). */
typedef struct
{
    float *samples;
    unsigned int num_samples;
    int samplerate;
} cached_sound_t;

typedef struct
{
    cached_sound_t *sound; /* NULL = channel idle */
    double pos;            /* playback position, in source samples */
    float left_gain, right_gain;
} mix_channel_t;

static boolean sound_initialized = false;
static boolean use_sfx_prefix;
static int mixer_freq;
static mix_channel_t channels[NUM_CHANNELS];
static float mixbuf[MIX_CHUNK_FRAMES * 2];

/* Per-channel gains from doom's volume (0..127, master SFX volume and
 * distance attenuation already folded in by s_sound.c) and separation
 * (0..254, 128 = centered) — i_sdlsound.c's Mix_SetPanning math, normalized
 * from 0..255 panning to 0..1 gain. */
static void ChannelGains(mix_channel_t *chan, int vol, int sep)
{
    int left = ((254 - sep) * vol) / 127;
    int right = (sep * vol) / 127;

    if (left < 0)
        left = 0;
    else if (left > 255)
        left = 255;
    if (right < 0)
        right = 0;
    else if (right > 255)
        right = 255;

    chan->left_gain = (float)left / 255.0f;
    chan->right_gain = (float)right / 255.0f;
}

/* Decode a DMX sound lump into a mono f32 cached_sound_t, or NULL if the
 * lump isn't a valid format-3 DMX sound. Layout (as parsed by i_sdlsound.c's
 * CacheSFX): u16 format (must be 3), u16 sample rate, u32 length, then 16
 * bytes of padding, `length - 32` bytes of 8-bit unsigned PCM, and 16 more
 * padding bytes counted inside `length`. */
static cached_sound_t *CacheSFX(sfxinfo_t *sfxinfo)
{
    int lumpnum;
    unsigned int lumplen;
    int samplerate;
    unsigned int length;
    byte *data;
    cached_sound_t *snd;
    unsigned int i;

    lumpnum = sfxinfo->lumpnum;
    data = W_CacheLumpNum(lumpnum, PU_STATIC);
    lumplen = W_LumpLength(lumpnum);

    if (lumplen < 8 || data[0] != 0x03 || data[1] != 0x00)
    {
        return NULL;
    }

    samplerate = (data[3] << 8) | data[2];
    length = (data[7] << 24) | (data[6] << 16) | (data[5] << 8) | data[4];

    /* Reject truncated lumps, and (like DMX) sounds shorter than 49
     * samples. */
    if (length > lumplen - 8 || length <= 48)
    {
        return NULL;
    }

    /* DMX skips the first and last 16 bytes of the sample data. */
    data += 16;
    length -= 32;

    snd = malloc(sizeof(cached_sound_t) + length * sizeof(float));

    if (snd == NULL)
    {
        return NULL;
    }

    snd->samples = (float *)(snd + 1);
    snd->num_samples = length;
    snd->samplerate = samplerate;

    /* 8-bit unsigned -> f32, expanded exactly like ExpandSoundData_SDL:
     * s16 = (u8 | u8 << 8) - 32768, then normalized. */
    for (i = 0; i < length; ++i)
    {
        byte b = (data + 8)[i];
        snd->samples[i] = (float)(((b << 8) | b) - 32768) / 32768.0f;
    }

    sfxinfo->driver_data = snd;

    /* The original lump isn't needed once decoded. */
    W_ReleaseLumpNum(lumpnum);

    return snd;
}

/* Mix one chunk of MIX_CHUNK_FRAMES stereo frames from the active channels
 * (linear-interpolation resample from each sound's native rate) and queue it
 * on the pcm-queue. */
static void MixChunk(void)
{
    int i;
    unsigned int frame;

    memset(mixbuf, 0, sizeof(mixbuf));

    for (i = 0; i < NUM_CHANNELS; ++i)
    {
        mix_channel_t *chan = &channels[i];
        cached_sound_t *snd = chan->sound;
        double step;

        if (snd == NULL)
        {
            continue;
        }

        step = (double)snd->samplerate / mixer_freq;

        for (frame = 0; frame < MIX_CHUNK_FRAMES; ++frame)
        {
            unsigned int idx = (unsigned int)chan->pos;
            float frac, sample;

            if (idx + 1 >= snd->num_samples)
            {
                chan->sound = NULL;
                break;
            }

            frac = (float)(chan->pos - idx);
            sample = snd->samples[idx]
                   + (snd->samples[idx + 1] - snd->samples[idx]) * frac;

            mixbuf[frame * 2] += sample * chan->left_gain;
            mixbuf[frame * 2 + 1] += sample * chan->right_gain;

            chan->pos += step;
        }
    }

    /* Clip the mix to [-1, 1]. */
    for (frame = 0; frame < MIX_CHUNK_FRAMES * 2; ++frame)
    {
        if (mixbuf[frame] > 1.0f)
        {
            mixbuf[frame] = 1.0f;
        }
        else if (mixbuf[frame] < -1.0f)
        {
            mixbuf[frame] = -1.0f;
        }
    }

    wkaudio_write(mixbuf, MIX_CHUNK_FRAMES);
}

static void GetSfxLumpName(sfxinfo_t *sfx, char *buf, size_t buf_len)
{
    /* Linked sfx lumps: use the sound linked to. */
    if (sfx->link != NULL)
    {
        sfx = sfx->link;
    }

    /* Doom prefixes sound lumps with "ds". */
    if (use_sfx_prefix)
    {
        M_snprintf(buf, buf_len, "ds%s", DEH_String(sfx->name));
    }
    else
    {
        M_StringCopy(buf, DEH_String(sfx->name), buf_len);
    }
}

static boolean I_WK_InitSound(boolean _use_sfx_prefix)
{
    use_sfx_prefix = _use_sfx_prefix;
    mixer_freq = snd_samplerate;

    if (wkaudio_open((float)mixer_freq, 2) < 0)
    {
        return false;
    }

    memset(channels, 0, sizeof(channels));
    sound_initialized = true;

    return true;
}

static void I_WK_ShutdownSound(void)
{
    sound_initialized = false;
}

static int I_WK_GetSfxLumpNum(sfxinfo_t *sfx)
{
    char namebuf[9];

    GetSfxLumpName(sfx, namebuf, sizeof(namebuf));

    return W_GetNumForName(namebuf);
}

static void I_WK_UpdateSound(void)
{
    if (!sound_initialized)
    {
        return;
    }

    while (wkaudio_buffered() < TARGET_BUFFERED_SECONDS)
    {
        MixChunk();
    }
}

static void I_WK_UpdateSoundParams(int channel, int vol, int sep)
{
    if (!sound_initialized || channel < 0 || channel >= NUM_CHANNELS)
    {
        return;
    }

    ChannelGains(&channels[channel], vol, sep);
}

static int I_WK_StartSound(sfxinfo_t *sfxinfo, int channel, int vol, int sep)
{
    cached_sound_t *snd;

    if (!sound_initialized || channel < 0 || channel >= NUM_CHANNELS)
    {
        return -1;
    }

    /* Load and decode the sound if this is its first play. */
    snd = sfxinfo->driver_data;

    if (snd == NULL)
    {
        snd = CacheSFX(sfxinfo);

        if (snd == NULL)
        {
            return -1;
        }
    }

    channels[channel].sound = NULL; /* cut whatever was playing here */
    channels[channel].pos = 0.0;
    ChannelGains(&channels[channel], vol, sep);
    channels[channel].sound = snd;

    return channel;
}

static void I_WK_StopSound(int channel)
{
    if (!sound_initialized || channel < 0 || channel >= NUM_CHANNELS)
    {
        return;
    }

    channels[channel].sound = NULL;
}

static boolean I_WK_SoundIsPlaying(int channel)
{
    if (!sound_initialized || channel < 0 || channel >= NUM_CHANNELS)
    {
        return false;
    }

    return channels[channel].sound != NULL;
}

static void I_WK_PrecacheSounds(sfxinfo_t *sounds, int num_sounds)
{
    /* No-op: sounds are decoded and cached on first play. */
}

static snddevice_t sound_wk_devices[] =
{
    SNDDEVICE_SB,
    SNDDEVICE_PAS,
    SNDDEVICE_GUS,
    SNDDEVICE_WAVEBLASTER,
    SNDDEVICE_SOUNDCANVAS,
    SNDDEVICE_AWE32,
};

sound_module_t DG_sound_module =
{
    sound_wk_devices,
    arrlen(sound_wk_devices),
    I_WK_InitSound,
    I_WK_ShutdownSound,
    I_WK_GetSfxLumpNum,
    I_WK_UpdateSound,
    I_WK_UpdateSoundParams,
    I_WK_StartSound,
    I_WK_StopSound,
    I_WK_SoundIsPlaying,
    I_WK_PrecacheSounds,
};

/* Music: a no-op module. i_sound.c unconditionally routes I_*Music/I_*Song
 * calls through DG_music_module when FEATURE_SOUND is on; OPL emulation is
 * out of scope, so every hook succeeds silently. */

static boolean I_WK_InitMusic(void)
{
    return true;
}

static void I_WK_ShutdownMusic(void)
{
}

static void I_WK_SetMusicVolume(int volume)
{
}

static void I_WK_PauseSong(void)
{
}

static void I_WK_ResumeSong(void)
{
}

static void *I_WK_RegisterSong(void *data, int len)
{
    return NULL;
}

static void I_WK_UnRegisterSong(void *handle)
{
}

static void I_WK_PlaySong(void *handle, boolean looping)
{
}

static void I_WK_StopSong(void)
{
}

static boolean I_WK_MusicIsPlaying(void)
{
    return false;
}

static void I_WK_PollMusic(void)
{
}

static snddevice_t music_wk_devices[] =
{
    SNDDEVICE_PAS,
    SNDDEVICE_GUS,
    SNDDEVICE_WAVEBLASTER,
    SNDDEVICE_SOUNDCANVAS,
    SNDDEVICE_GENMIDI,
    SNDDEVICE_AWE32,
};

music_module_t DG_music_module =
{
    music_wk_devices,
    arrlen(music_wk_devices),
    I_WK_InitMusic,
    I_WK_ShutdownMusic,
    I_WK_SetMusicVolume,
    I_WK_PauseSong,
    I_WK_ResumeSong,
    I_WK_RegisterSong,
    I_WK_UnRegisterSong,
    I_WK_PlaySong,
    I_WK_StopSong,
    I_WK_MusicIsPlaying,
    I_WK_PollMusic,
};
