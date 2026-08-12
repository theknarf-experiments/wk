/* <sys/stat.h> wrapper for wasm32-wasi: WASI has no process-wide file-creation
 * mask, so umask() isn't declared. coreutils (chmod, install, mkdir, ...)
 * calls it; compat.c keeps a process-local mask so the value round-trips. */
#ifndef _WK_COMPAT_SYS_STAT_H
#define _WK_COMPAT_SYS_STAT_H

#include_next <sys/stat.h>

mode_t umask(mode_t mask);

#endif
