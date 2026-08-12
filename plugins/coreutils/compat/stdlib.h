/* <stdlib.h> wrapper for wasm32-wasi: wasi-libc has no qsort_r (the
 * context-passing sort gnulib's savedir uses). Implemented in compat.c on top
 * of a thread-local context — safe here because a wasm guest is
 * single-threaded and coreutils never sorts re-entrantly. */
#ifndef _WK_COMPAT_STDLIB_H
#define _WK_COMPAT_STDLIB_H

#include_next <stdlib.h>

void qsort_r(void *base, size_t nmemb, size_t size,
             int (*compar)(const void *, const void *, void *), void *arg);

#endif
