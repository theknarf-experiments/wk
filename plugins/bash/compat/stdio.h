/* <stdio.h> wrapper for wasm32-wasi: single-threaded, so the per-FILE locking
 * API (flockfile/funlockfile/ftrylockfile, used by gnulib's getopt and by
 * several coreutils fast paths) has nothing to guard. Declared here,
 * implemented as no-ops in compat.c. */
#ifndef _WK_COMPAT_STDIO_H
#define _WK_COMPAT_STDIO_H

#include_next <stdio.h>

void flockfile(FILE *stream);
void funlockfile(FILE *stream);
int ftrylockfile(FILE *stream);

#endif
