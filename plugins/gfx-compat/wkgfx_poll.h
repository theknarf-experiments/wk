/* wkgfx_poll.h — the wk frame as a FILE DESCRIPTOR, so that one poll() waits
 * on the host's next frame, a deadline, and a pile of sockets at once.
 *
 * THE PROBLEM. A guest with a real event loop (Qt's QWkEventDispatcher) has
 * exactly one place where the process is allowed to block, and everything it
 * can wake on has to meet there: the host frame, the nearest timer deadline,
 * and — the moment QSocketNotifier exists — every socket the application is
 * watching. wkgfx_wait_frame_timeout() already builds that meeting point by
 * hand out of wasi:io/poll: [frame pollable, deadline pollable]. The obvious
 * next step is to append the sockets' pollables to that list.
 *
 * THAT DIRECTION IS CLOSED, and it is worth writing down why so nobody spends
 * another day on it. On wasip2 a socket is a libc file descriptor, and the
 * wasi:io/poll pollable behind it is not reachable from outside libc:
 *
 *   * the descriptor vtable's only pollable-producing hook is
 *     poll_register(void *, poll_state_t *, short), and `poll_state_t` is an
 *     INCOMPLETE type whose definition lives inside libc-bottom-half's
 *     ppoll.c. There is no accessor;
 *   * the one public-ish route, get_read_stream() -> subscribe(), yields a
 *     pollable only for a CONNECTED socket. A connecting socket and a
 *     listening socket both fail it with ENOTCONN — i.e. exactly the two
 *     states QTcpSocket::connectToHost() and QTcpServer::listen() need.
 *
 * THE INVERSION. So the frame travels the other way: this shim wraps the
 * frame pollable in a descriptor of wk's own and puts it in libc's descriptor
 * table, which makes it an ordinary fd. Then ppoll() over [notifier fds...,
 * frame fd] IS the single wasi:io/poll call — libc's ppoll_impl asks every
 * descriptor to register its pollables, appends a
 * monotonic-clock.subscribe-duration for the timeout, and makes ONE
 * wasi:io/poll.poll over the lot. Same list wkgfx assembled by hand, built by
 * libc instead, and sockets get in for free through wasi-libc's own
 * tcp_poll_register — which handles connecting, listening and connected
 * sockets alike, and even completes finish-connect for us.
 *
 * This is ../pipe-compat's trick (a wk channel behind a libc fd) applied to a
 * pollable instead of a stream, and it inherits pipe-compat's fragility: the
 * descriptor table's layout is PRIVATE to wasi-libc and is transcribed, not
 * included. A mismatch is silent corruption, not a link error. The single
 * transcription in ../pipe-compat/wasilibc_descriptor_table.h is shared rather
 * than copied for exactly that reason, and every build that compiles
 * wkgfx_poll.c must carry the wasi-sdk-34-rc.2 guard that pipe-compat's
 * build.sh and plugins/qt/build-qpa.sh both have.
 *
 * Deliberately NOT part of wkgfx.c: every other gfx-compat consumer (doom,
 * netsurf, mupdf, ...) compiles wkgfx.c, and none of them should inherit a
 * dependency on libc internals to draw a rectangle.
 */
#ifndef WKGFX_POLL_H
#define WKGFX_POLL_H

#include <poll.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* A file descriptor that becomes readable when the host signals a frame.
 *
 * Poll it with POLLIN; when it comes back readable, call wkgfx_take_frame()
 * (wkgfx.h) — the readiness itself was already consumed by the poll, but
 * get-frame is what traps on a closed surface, so skipping it turns a closed
 * window into a busy loop.
 *
 * The same fd every call; -1 with errno set before wkgfx_open() or if libc's
 * descriptor table is full. Reading, writing or seeking it is meaningless and
 * libc reports the right errno by itself: this descriptor is nothing but a
 * pollable wearing an fd. */
int wkgfx_frame_fd(void);

#ifdef __cplusplus
}
#endif

#endif /* WKGFX_POLL_H */
