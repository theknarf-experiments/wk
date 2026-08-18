/* compat.c — definitions for the wasi-libc gaps declared in wk_compat.h,
 * linked into the viewer alongside libmupdf.a (the lua compat.c pattern).
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "wk_compat.h"

/* A real mkstemp over open(O_CREAT|O_EXCL): wasi-libc omits it ("WASI has no
 * temp directories"), but wk nodes have a writable vfs root, so the usual
 * replace-the-XXXXXX dance works fine. Only fz_new_output_to_tempfile calls
 * it, and only for accelerator files this viewer never asks for. */
int mkstemp(char *template_) {
    static const char letters[] =
        "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    size_t len = strlen(template_);
    if (len < 6 || strcmp(template_ + len - 6, "XXXXXX") != 0) {
        errno = EINVAL;
        return -1;
    }
    char *x = template_ + len - 6;
    struct timespec ts = {0, 0};
    clock_gettime(CLOCK_REALTIME, &ts);
    unsigned seed =
        (unsigned)ts.tv_nsec ^ (unsigned)ts.tv_sec ^ (unsigned)(uintptr_t)template_;
    for (int attempt = 0; attempt < 100; attempt++) {
        for (int i = 0; i < 6; i++) {
            seed = seed * 1103515245u + 12345u;
            x[i] = letters[(seed >> 16) % (sizeof(letters) - 1)];
        }
        int fd = open(template_, O_RDWR | O_CREAT | O_EXCL, 0600);
        if (fd >= 0)
            return fd;
        if (errno != EEXIST)
            return -1;
    }
    errno = EEXIST;
    return -1;
}
