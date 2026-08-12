/* <time.h> wrapper for wasm32-wasi: WASI has no timezone database, so
 * tzset()/tzname aren't declared. gnulib's mktime.c calls tzset()
 * unconditionally; compat.c makes it a no-op (everything is UTC here). */
#ifndef _WK_COMPAT_TIME_H
#define _WK_COMPAT_TIME_H

#include_next <time.h>

void tzset(void);

#endif
