/* The parts of wasi-libc's private descriptor table that a guest needs in
 * order to add a file descriptor of its own.
 *
 * WHY THIS FILE EXISTS
 *
 * On wasip2 the file-descriptor table lives in wasi-libc, in guest memory, and
 * an entry is a fat pointer: some data plus a vtable of operations. That is an
 * extension point — a descriptor whose read and write streams are ordinary
 * `wasi:io` streams is indistinguishable to libc from stdin or a socket, so
 * `read`, `write`, `poll`, `close`, `dup` and `fstat` on it all work with no
 * further help from us. It is how wasi-libc implements stdio itself
 * (libc-bottom-half/sources/wasip2_stdio.c), and how it implements `pipe` on
 * wasip3, where the component model has async streams to build one from. On
 * wasip2 there is no `pipe`, so wk supplies the channel (`wk:exec`'s pipe) and
 * this is how it gets a file descriptor.
 *
 * WHY IT IS COPIED RATHER THAN INCLUDED
 *
 * These declarations are private: wasi-libc keeps them in
 * libc-bottom-half/headers/private/ and the sysroot does not ship them. The
 * upstream header also pulls in more private headers (wasi/poll.h, lock.h), so
 * only what is actually needed is reproduced here.
 *
 * THE LAYOUT MUST MATCH THE LIBC WE LINK AGAINST. These structs are written
 * and read by prebuilt libc objects, so a field in the wrong place is silent
 * corruption rather than a link error. They are transcribed from wasi-libc
 *
 *     fb2edcef33395cd18f5921041f9c40d6127adc68
 *
 * which is the revision in wasi-sdk-34-rc.2, the version pinned in the root
 * mise.toml. build.sh checks that pin and refuses to build against a different
 * toolchain, because that check is the only thing standing between a wasi-libc
 * bump and a very confusing bug.
 */
#ifndef WK_WASILIBC_DESCRIPTOR_TABLE_H
#define WK_WASILIBC_DESCRIPTOR_TABLE_H

#include <stdbool.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <wasi/wasip2.h>

/* Opaque here: only ever handled as a pointer, by `poll`, which we don't
 * implement (leaving poll_register null makes libc derive readiness from the
 * read and write streams, which is exactly right for a pipe). */
typedef struct poll_state_t poll_state_t;

/* What libc wants filled in to read from this descriptor. */
typedef struct wasi_read_t {
    /* An optional pointer to the descriptor's own offset, updated on a
     * successful read. A pipe has no offset, so this stays null. */
    off_t *offset;
    bool blocking;
    monotonic_clock_duration_t timeout;
    streams_borrow_input_stream_t input;
    /* Somewhere libc can cache a pollable derived from `input`; it fills this
     * in lazily, so it must point at storage that outlives the call. */
    poll_own_pollable_t *pollable;
} wasi_read_t;

/* The same, for writing. */
typedef struct wasi_write_t {
    off_t *offset;
    bool blocking;
    monotonic_clock_duration_t timeout;
    streams_borrow_output_stream_t output;
    poll_own_pollable_t *pollable;
} wasi_write_t;

/* The operations of a descriptor. Order matters — libc calls through this by
 * offset — so every member is kept, in place, even the ones a pipe leaves
 * null. A null entry means "not supported", which libc reports as the right
 * errno by itself. */
typedef struct descriptor_vtable_t {
    void (*free)(void *);

    int (*get_read_stream)(void *, wasi_read_t *);
    int (*get_write_stream)(void *, wasi_write_t *);
    int (*set_blocking)(void *, bool);
    int (*fstat)(void *, struct stat *);

    int (*get_file)(void *, filesystem_borrow_descriptor_t *);
    off_t (*seek)(void *, off_t, int);
    int (*fcntl_getfl)(void *);
    int (*fcntl_setfl)(void *, int);
    int (*isatty)(void *);

    int (*accept4)(void *, struct sockaddr *, socklen_t *, int);
    int (*bind)(void *, const struct sockaddr *, socklen_t);
    int (*connect)(void *, const struct sockaddr *, socklen_t);
    int (*getsockname)(void *, struct sockaddr *, socklen_t *);
    int (*getpeername)(void *, struct sockaddr *, socklen_t *);
    int (*listen)(void *, int);
    ssize_t (*recvfrom)(void *, void *, size_t, int, struct sockaddr *, socklen_t *);
    ssize_t (*sendto)(void *, const void *, size_t, int, const struct sockaddr *, socklen_t);
    int (*shutdown)(void *, int);
    int (*getsockopt)(void *, int, int, void *, socklen_t *);
    int (*setsockopt)(void *, int, int, const void *, socklen_t);

    int (*poll_register)(void *, poll_state_t *, short);
    int (*poll_finish)(void *, poll_state_t *, short);
} descriptor_vtable_t;

/* Every descriptor's data begins with this; libc does the counting, which is
 * what makes `dup` on our descriptor work like `dup` on any other. */
typedef struct {
    unsigned cnt;
} descriptor_refcnt_t;

typedef struct {
    descriptor_refcnt_t *data;
    descriptor_vtable_t *vtable;
} descriptor_table_entry_t;

/* Adds `entry` to the table and returns its file descriptor, or -1 with errno
 * set (having run the entry's destructor). The entry is expected to arrive
 * with a reference count of 0; this initialises it. */
extern int descriptor_table_insert(descriptor_table_entry_t entry);

#endif /* WK_WASILIBC_DESCRIPTOR_TABLE_H */
