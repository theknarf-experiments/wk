/* <dirent.h> wrapper for wasm32-wasi.
 *
 * Name collision: wasi-libc declares its own `opendirat(int, const char *)`
 * (a WASI extension), while gnulib defines a different 4-argument
 * `opendirat` used by backupfile.c. Rename wasi-libc's out of the way as its
 * header is pulled in — nothing here calls it — leaving the name to gnulib.
 */
#ifndef _WK_COMPAT_DIRENT_H
#define _WK_COMPAT_DIRENT_H

#define opendirat __wasilibc_opendirat
#include_next <dirent.h>
#undef opendirat

#endif
