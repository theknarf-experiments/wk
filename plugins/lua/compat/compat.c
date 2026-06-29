/* Tiny libc stubs for the three functions wasi-libc deliberately omits but the
 * unmodified Lua standard library references, so Lua links against a WASI
 * sysroot. None are meaningful in wk's sandbox, so they fail gracefully rather
 * than do anything: io.tmpfile()/os.tmpname() return nil/error, os.execute()
 * reports "no shell". This keeps lua.c and the rest of Lua source unmodified —
 * the missing surface is supplied by the runtime, not patched into the app. */
#include <stdio.h>
#include <stdlib.h>

FILE *tmpfile(void) { return NULL; }

int system(const char *cmd) {
    (void)cmd;
    return -1;
}

char *tmpnam(char *s) {
    (void)s;
    return NULL;
}
