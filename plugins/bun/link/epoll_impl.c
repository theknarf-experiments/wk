// epoll(2) emulated over poll(2) for wasi. bun's uSockets loop drives all I/O
// through epoll; wasi has no epoll, but wasi-libc's poll() (poll_oneoff) does
// report readiness for socket fds backed by wasi:sockets. So epoll_ctl records
// each fd's requested events + opaque data, and epoll_pwait polls the recorded
// fds and hands back epoll_events — making bun's HTTP server/client actually
// respond in a wk node wired to a Network. With no fds registered it just
// honors the timeout, which is how the setTimeout/setInterval wait is driven.
//
// Level-triggered (poll's semantics): uSockets requests EPOLLET but its
// correctness doesn't depend on edge triggering — it drains reads until EAGAIN
// and only keeps EPOLLOUT registered while backpressured, so level-triggered
// reporting doesn't busy-spin in the common case.
#include <sys/epoll.h>
#include <poll.h>
#include <time.h>
#include <unistd.h>
#include <errno.h>

// One shared table for the whole process. bun runs one or two uSockets loops
// (each its own epoll fd); entries are keyed by (epfd, fd).
#define WK_EP_MAX 4096
struct wk_ep_ent {
    int epfd;
    int fd;
    unsigned int events; // epoll events requested for this fd
    epoll_data_t data;   // opaque cookie handed back on readiness (us_poll_t*)
    int used;
};
static struct wk_ep_ent g_ent[WK_EP_MAX];
// High-water mark of used slots, to bound the per-wait scan.
static int g_hi = 0;

int epoll_create1(int flags) {
    (void)flags;
    // A real, closeable fd, distinct per loop — used only as the table key.
    int fd = dup(2);
    return fd < 0 ? -1 : fd;
}

int epoll_ctl(int epfd, int op, int fd, struct epoll_event *ev) {
    if (op == EPOLL_CTL_DEL) {
        for (int i = 0; i < g_hi; i++)
            if (g_ent[i].used && g_ent[i].epfd == epfd && g_ent[i].fd == fd) {
                g_ent[i].used = 0;
                return 0;
            }
        errno = ENOENT;
        return -1;
    }
    if (op == EPOLL_CTL_MOD) {
        for (int i = 0; i < g_hi; i++)
            if (g_ent[i].used && g_ent[i].epfd == epfd && g_ent[i].fd == fd) {
                g_ent[i].events = ev ? ev->events : 0;
                if (ev) g_ent[i].data = ev->data;
                return 0;
            }
        errno = ENOENT;
        return -1;
    }
    // EPOLL_CTL_ADD
    for (int i = 0; i < g_hi; i++)
        if (g_ent[i].used && g_ent[i].epfd == epfd && g_ent[i].fd == fd) {
            errno = EEXIST;
            return -1;
        }
    for (int i = 0; i < WK_EP_MAX; i++)
        if (!g_ent[i].used) {
            g_ent[i].used = 1;
            g_ent[i].epfd = epfd;
            g_ent[i].fd = fd;
            g_ent[i].events = ev ? ev->events : 0;
            if (ev) g_ent[i].data = ev->data;
            if (i >= g_hi) g_hi = i + 1;
            return 0;
        }
    errno = ENOSPC;
    return -1;
}

int epoll_pwait(int epfd, struct epoll_event *events, int maxevents, int timeout,
                const void *sigmask) {
    (void)sigmask;
    struct pollfd pfds[WK_EP_MAX];
    int slot[WK_EP_MAX];
    int n = 0;
    for (int i = 0; i < g_hi; i++) {
        if (!(g_ent[i].used && g_ent[i].epfd == epfd)) continue;
        short pe = 0;
        if (g_ent[i].events & EPOLLIN) pe |= POLLIN;
        if (g_ent[i].events & EPOLLOUT) pe |= POLLOUT;
#ifdef POLLRDHUP
        if (g_ent[i].events & EPOLLRDHUP) pe |= POLLRDHUP;
#endif
        pfds[n].fd = g_ent[i].fd;
        pfds[n].events = pe;
        pfds[n].revents = 0;
        slot[n] = i;
        n++;
    }

    if (n == 0) {
        // Timer-only wait: honor the timeout, report nothing. A negative
        // (infinite) timeout has no I/O source to wake it here, and bun always
        // folds a finite next-timer expiry into `timeout`, so return at once
        // rather than block forever.
        if (timeout > 0) {
            struct timespec ts = {.tv_sec = timeout / 1000,
                                  .tv_nsec = (long)(timeout % 1000) * 1000000L};
            nanosleep(&ts, 0);
        }
        return 0;
    }

    int r = poll(pfds, n, timeout);
    if (r < 0) {
        // wasi-libc's poll_oneoff rejects wasip2 socket fds (ENOTSUP), so real
        // readiness notification isn't available here. Fall back to
        // level-triggered busy-polling: report each fd's requested events as
        // ready and let uSockets attempt the accept/recv/send (which returns
        // EAGAIN when not actually ready). A 1ms floor bounds CPU, and timers
        // still fire every tick since bun re-checks expiry each loop iteration.
        struct timespec ts = {.tv_sec = 0, .tv_nsec = 1000000L};
        nanosleep(&ts, 0);
        int out = 0;
        for (int i = 0; i < n && out < maxevents; i++) {
            events[out].events = g_ent[slot[i]].events & (EPOLLIN | EPOLLOUT);
            events[out].data = g_ent[slot[i]].data;
            out++;
        }
        return out;
    }
    if (r == 0) return 0;

    int out = 0;
    for (int i = 0; i < n && out < maxevents; i++) {
        short re = pfds[i].revents;
        if (!re) continue;
        unsigned int ee = 0;
        if (re & POLLIN) ee |= EPOLLIN;
        if (re & POLLOUT) ee |= EPOLLOUT;
        if (re & POLLERR) ee |= EPOLLERR;
        if (re & POLLHUP) ee |= EPOLLHUP;
        if (re & POLLNVAL) ee |= EPOLLERR;
#ifdef POLLRDHUP
        if (re & POLLRDHUP) ee |= EPOLLRDHUP;
#endif
        events[out].events = ee;
        events[out].data = g_ent[slot[i]].data;
        out++;
    }
    return out;
}

int epoll_wait(int epfd, struct epoll_event *events, int maxevents, int timeout) {
    return epoll_pwait(epfd, events, maxevents, timeout, 0);
}
