/* A real `pipe()` for wk guests.
 *
 * wasi-libc's wasip2 `pipe` returns ENOSYS: the component model had no way to
 * build one until wasip3's async streams, so on wasip2 there is nothing to
 * implement it with. wk has something — `wk:exec`'s pipe is a bounded buffer
 * with two ends, and it hands each end out as an ordinary `wasi:io` stream —
 * so this fills the gap by putting those streams behind a file descriptor.
 *
 * Once inserted, the descriptor is a descriptor: `read`, `write`, `close`,
 * `dup`/`dup2`, `poll` and `fcntl` are libc's own, and nothing linking this
 * shim needs to know a pipe is involved. That is the point — the capability
 * belongs to every guest, not to one patched program.
 *
 * What a pipe deliberately does not have: no seek, no file, no socket
 * operations. Those vtable slots stay null, which is how libc knows to report
 * ESPIPE/ENOTSOCK itself. `poll_register` is null too, so libc derives
 * readiness from the read and write streams — exactly the right behaviour, and
 * why `poll()` on a pipe works without another line here.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "exec_host.h"
#include "wasilibc_descriptor_table.h"

/* The wk pipe both ends refer to. Held apart from the ends and counted,
 * because either end may be closed first and the handle has to outlive both:
 * it is also what `wk_pipe_of_fd` hands back so a caller can wire the pipe to
 * a spawned program. */
typedef struct {
    unsigned refs;
    wk_exec_process_own_pipe_t pipe;
} pipe_shared_t;

/* One end of a pipe, as a descriptor. Both ends are the same shape; which one
 * this is depends on which stream handle is set. */
typedef struct {
    descriptor_refcnt_t refcnt; /* must be first: libc counts through it */
    streams_own_input_stream_t input;
    streams_own_output_stream_t output;
    /* Storage for the pollable libc caches; it fills these in lazily. */
    poll_own_pollable_t input_pollable;
    poll_own_pollable_t output_pollable;
    pipe_shared_t *shared;
} pipe_end_t;

static void pipe_end_free(void *data) {
    pipe_end_t *e = data;
    if (e->input_pollable.__handle != 0)
        poll_pollable_drop_own(e->input_pollable);
    if (e->output_pollable.__handle != 0)
        poll_pollable_drop_own(e->output_pollable);
    /* Dropping the stream is what tells wk this end has gone — which is how
     * the other side ever sees end-of-file. */
    if (e->input.__handle != 0)
        streams_input_stream_drop_own(e->input);
    if (e->output.__handle != 0)
        streams_output_stream_drop_own(e->output);
    if (e->shared && --e->shared->refs == 0) {
        wk_exec_process_pipe_drop_own(e->shared->pipe);
        free(e->shared);
    }
    free(e);
}

static int pipe_get_read_stream(void *data, wasi_read_t *read) {
    pipe_end_t *e = data;
    if (e->input.__handle == 0) {
        errno = EBADF; /* the write end: not readable */
        return -1;
    }
    read->input = streams_borrow_input_stream(e->input);
    read->offset = NULL; /* a pipe has no position */
    read->pollable = &e->input_pollable;
    read->timeout = 0;
    read->blocking = true;
    return 0;
}

static int pipe_get_write_stream(void *data, wasi_write_t *write) {
    pipe_end_t *e = data;
    if (e->output.__handle == 0) {
        errno = EBADF; /* the read end: not writable */
        return -1;
    }
    write->output = streams_borrow_output_stream(e->output);
    write->offset = NULL;
    write->pollable = &e->output_pollable;
    write->timeout = 0;
    write->blocking = true;
    return 0;
}

static int pipe_fstat(void *data, struct stat *buf) {
    pipe_end_t *e = data;
    memset(buf, 0, sizeof *buf);
    /* Programs branch on this: `S_ISFIFO` is how they tell a pipe from a
     * regular file and decide not to seek. */
    buf->st_mode = S_IFIFO | (e->input.__handle ? 0400 : 0200);
    buf->st_nlink = 1;
    return 0;
}

static int pipe_fcntl_getfl(void *data) {
    pipe_end_t *e = data;
    return e->input.__handle ? O_RDONLY : O_WRONLY;
}

static int pipe_isatty(void *data) {
    (void)data;
    return 0;
}

static descriptor_vtable_t pipe_vtable = {
    .free = pipe_end_free,
    .get_read_stream = pipe_get_read_stream,
    .get_write_stream = pipe_get_write_stream,
    .fstat = pipe_fstat,
    .fcntl_getfl = pipe_fcntl_getfl,
    .isatty = pipe_isatty,
};

/* Insert one end, taking ownership of the stream handle either way. */
static int insert_end(streams_own_input_stream_t *in,
                      streams_own_output_stream_t *out, pipe_shared_t *shared) {
    pipe_end_t *e = calloc(1, sizeof *e);
    if (!e) {
        errno = ENOMEM;
        return -1;
    }
    if (in)
        e->input = *in;
    if (out)
        e->output = *out;
    e->shared = shared;
    shared->refs++;

    descriptor_table_entry_t entry = {(descriptor_refcnt_t *)e, &pipe_vtable};
    /* On failure this has already run our destructor, so the streams are
     * released and there is nothing left to clean up here. */
    return descriptor_table_insert(entry);
}

int pipe(int fds[2]) {
    wk_exec_process_own_pipe_t p = wk_exec_process_constructor_pipe();
    wk_exec_process_borrow_pipe_t b = wk_exec_process_borrow_pipe(p);

    /* Two generators describe the same handle: wit-bindgen names it
     * wk_exec_process_own_input_stream_t here, the sysroot's own bindings call
     * it streams_own_input_stream_t. A resource handle is an i32 either way,
     * so carry it across by value rather than casting the structs. */
    wk_exec_process_own_input_stream_t rin = wk_exec_process_method_pipe_read_end(b);
    wk_exec_process_own_output_stream_t rout = wk_exec_process_method_pipe_write_end(b);
    streams_own_input_stream_t in = {rin.__handle};
    streams_own_output_stream_t out = {rout.__handle};

    pipe_shared_t *shared = calloc(1, sizeof *shared);
    if (!shared) {
        streams_input_stream_drop_own(in);
        streams_output_stream_drop_own(out);
        wk_exec_process_pipe_drop_own(p);
        errno = ENOMEM;
        return -1;
    }
    shared->pipe = p;

    int rfd = insert_end(&in, NULL, shared);
    if (rfd < 0) {
        streams_output_stream_drop_own(out);
        return -1;
    }
    int wfd = insert_end(NULL, &out, shared);
    if (wfd < 0) {
        close(rfd);
        return -1;
    }
    fds[0] = rfd;
    fds[1] = wfd;
    return 0;
}

/* If `fd` is one of our pipe ends, hand back a borrow of the wk pipe behind
 * it. That is what lets a caller wire this very pipe to a program it spawns —
 * the shell's `a | b`, where the stage's stdio has to be the pipe rather than
 * a copy of its bytes. Returns false for any other descriptor. */
bool wk_pipe_of_fd(int fd, wk_exec_process_borrow_pipe_t *out) {
    descriptor_table_entry_t entry;
    if (descriptor_table_get(fd, &entry) != 0)
        return false;

    /* The lookup took a reference; every path below has to give it back, or
     * the descriptor outlives its close and the pipe never reports EOF. */
    bool ours = entry.vtable == &pipe_vtable; /* not a file/socket/terminal */
    if (ours) {
        pipe_end_t *e = (pipe_end_t *)entry.data;
        ours = e->shared != NULL;
        if (ours)
            *out = wk_exec_process_borrow_pipe(e->shared->pipe);
    }
    descriptor_table_entry_dec(entry);
    return ours;
}

int pipe2(int fds[2], int flags) {
    /* O_NONBLOCK would need set_blocking plumbed through the vtable; O_CLOEXEC
     * is meaningless without exec-in-place. Refuse rather than pretend. */
    if (flags & ~O_CLOEXEC) {
        errno = EINVAL;
        return -1;
    }
    return pipe(fds);
}
