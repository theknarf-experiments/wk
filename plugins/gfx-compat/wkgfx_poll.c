/* wkgfx_poll.c — see wkgfx_poll.h for why the frame becomes an fd rather than
 * the sockets becoming pollables. */
#include "wkgfx_poll.h"

#include "wkgfx.h"

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdlib.h>
#include <string.h>

/* ONE transcription of wasi-libc's private descriptor table, shared with
 * pipe-compat rather than copied. Two copies would drift, and drift here is
 * memory corruption with no diagnostic. */
#include "../pipe-compat/wasilibc_descriptor_table.h"

/* libc-bottom-half/headers/private/wasi/poll.h, which the sysroot does not
 * ship. Both are real exported symbols in libc.a (`llvm-nm --defined-only
 * libc.a` shows `T __wasilibc_poll_add` / `T __wasilibc_poll_ready` in
 * ppoll.c.obj), and — the point — they take `poll_state_t *` OPAQUELY. So a
 * descriptor of ours can contribute a pollable to somebody else's poll set
 * without knowing one field of that private struct's layout. Only the
 * descriptor table itself (above) is layout-sensitive.
 *
 * wasip3 changes both signatures (a callback form, and no borrow pollable);
 * this file is wasip2-only and says so. */
extern int __wasilibc_poll_add(poll_state_t *state, short events,
                               poll_borrow_pollable_t pollable);
extern void __wasilibc_poll_ready(poll_state_t *state, short events);

#ifndef __wasip2__
#error "wkgfx_poll.c is wasip2-only: wasip3 replaces __wasilibc_poll_add"
#endif

/* The descriptor's payload. It is only a refcount: the pollable itself belongs
 * to wkgfx and outlives every fd, so this borrows it by handle each time
 * rather than owning a copy that a close() could drop out from under the
 * surface. */
typedef struct {
    descriptor_refcnt_t refcnt; /* must be first: libc counts through it */
} frame_desc_t;

static void frame_free(void *data) {
    free(data);
}

static int frame_fstat(void *data, struct stat *buf) {
    (void)data;
    memset(buf, 0, sizeof *buf);
    /* A FIFO is the closest true thing: no size, no seek, readable. Programs
     * that stat an fd before deciding how to read it get a sane answer. */
    buf->st_mode = S_IFIFO | 0400;
    buf->st_nlink = 1;
    return 0;
}

static int frame_fcntl_getfl(void *data) {
    (void)data;
    return O_RDONLY;
}

static int frame_isatty(void *data) {
    (void)data;
    return 0;
}

/* The whole reason this descriptor exists.
 *
 * `events` is what the caller asked for, unfiltered — POLLPRI and the
 * output-only bits included — and on wasip2 __wasilibc_poll_ready does NOT
 * mask what it is given against pollfd->events (the wasip3 branch does). So
 * mask here: a frame is a read, and nothing else, and reporting POLLOUT on it
 * would make a caller that polls for writability spin.
 *
 * Returning 0 without adding anything (when POLLIN was not requested) is
 * correct and is what a POSIX poll does with an fd it can never satisfy: it
 * simply never reports it ready. */
static int frame_poll_register(void *data, poll_state_t *state, short events) {
    (void)data;
    const short want = events & POLLRDNORM;
    if (want == 0)
        return 0;
    const uint32_t handle = wkgfx_frame_pollable();
    if (handle == 0) {
        /* The surface was never opened, or has been torn down. */
        errno = EBADF;
        return -1;
    }
    poll_borrow_pollable_t borrow = {handle};
    return __wasilibc_poll_add(state, want, borrow);
}

/* poll_finish is deliberately null: with no finish hook libc calls
 * __wasilibc_poll_ready(state, <the events we registered>) itself, which is
 * exactly POLLRDNORM. There is nothing to complete — unlike a socket, where
 * finish-connect happens in that hook. */
static descriptor_vtable_t frame_vtable = {
    .free = frame_free,
    .fstat = frame_fstat,
    .fcntl_getfl = frame_fcntl_getfl,
    .isatty = frame_isatty,
    .poll_register = frame_poll_register,
};

static int g_frame_fd = -1;

int wkgfx_frame_fd(void) {
    if (g_frame_fd >= 0)
        return g_frame_fd;
    if (wkgfx_frame_pollable() == 0) {
        errno = EBADF; /* before wkgfx_open() there is no frame to wait for */
        return -1;
    }
    frame_desc_t *f = calloc(1, sizeof *f);
    if (!f) {
        errno = ENOMEM;
        return -1;
    }
    descriptor_table_entry_t entry = {(descriptor_refcnt_t *)f, &frame_vtable};
    /* On failure libc has already run our destructor. */
    g_frame_fd = descriptor_table_insert(entry);
    return g_frame_fd;
}
