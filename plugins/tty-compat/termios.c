/* Portable termios for wasm32-wasi, backed by wk's `wk:tty/control` capability.
 *
 * This is the ONLY place that knows the capability exists: it maps the POSIX
 * termios surface an unmodified terminal app expects onto get()/set(). When
 * WASI standardizes a terminal interface, only this file changes.
 *
 * The line discipline lives host-side; the two bits that cross the boundary are
 * ECHO (echo typed characters) and ICANON (cooked, line-buffered input). "Raw"
 * mode is simply both cleared. Everything else in struct termios is presented
 * with conventional values so apps that save/modify/restore it behave normally;
 * only ECHO and ICANON actually take effect. */

#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <termios.h>
#include <unistd.h>
#include <wasi/api.h>

#include "terminal.h" /* wit-bindgen: wk_tty_control_get / _set / _state_t */

int tcgetattr(int fd, struct termios *t) {
    (void)fd;
    if (!t) {
        errno = EINVAL;
        return -1;
    }
    wk_tty_control_state_t st;
    wk_tty_control_get(&st);

    /* Conventional cooked-mode flag values; ECHO/ICANON reflect the real state
     * so a save/restore round-trips. */
    t->c_iflag = BRKINT | ICRNL | INPCK | ISTRIP | IXON;
    t->c_oflag = OPOST | ONLCR;
    t->c_cflag = CS8 | CREAD | CLOCAL;
    t->c_lflag = ISIG | IEXTEN;
    if (st.echo)
        t->c_lflag |= ECHO | ECHOE | ECHOK;
    if (st.canonical)
        t->c_lflag |= ICANON;
    t->c_line = 0;
    t->c_ispeed = 0;
    t->c_ospeed = 0;

    /* Standard control characters. VERASE is DEL (0x7f), the conventional erase
     * key and what wk's terminal sends for Backspace — an app that adopts
     * c_cc[VERASE] as its backspace key (e.g. vim) then recognizes it. */
    for (int i = 0; i < NCCS; i++)
        t->c_cc[i] = 0;
    t->c_cc[VINTR] = 0x03;  /* Ctrl-C */
    t->c_cc[VQUIT] = 0x1c;  /* Ctrl-\ */
    t->c_cc[VERASE] = 0x7f; /* DEL */
    t->c_cc[VKILL] = 0x15;  /* Ctrl-U */
    t->c_cc[VEOF] = 0x04;   /* Ctrl-D */
    t->c_cc[VSTART] = 0x11; /* Ctrl-Q */
    t->c_cc[VSTOP] = 0x13;  /* Ctrl-S */
    t->c_cc[VSUSP] = 0x1a;  /* Ctrl-Z */
    t->c_cc[VMIN] = 1;
    t->c_cc[VTIME] = 0;
    return 0;
}

int tcsetattr(int fd, int optional_actions, const struct termios *t) {
    (void)fd;
    (void)optional_actions;
    if (!t) {
        errno = EINVAL;
        return -1;
    }
    wk_tty_control_set((t->c_lflag & ECHO) != 0, (t->c_lflag & ICANON) != 0);
    return 0;
}

/* SIGWINCH delivery. WASI has no async signals and no termios VTIME, and the
 * WASIp1 adapter turns an empty stream read into a retry (never a 0-byte
 * return), so the host can't asynchronously wake a blocked wait. Instead the
 * shim drives delivery from the guest side: every blocking wait on stdin
 * (read/poll/select) is capped to a short interval, and on each timeout it
 * re-reads the window size from wk:tty/control. When it changed, it raises the
 * SIGWINCH an unmodified terminal app installs a handler for — the app then
 * re-queries the size (ioctl/wk:tty) and redraws. This is app-agnostic: kilo
 * blocks in read(), vim in select(); both funnel through the wait below. */
#define WK_WINCH_POLL_MS 100
#define WK_MAXFDS 16

static unsigned wk_win_cols, wk_win_rows;
static int wk_win_known;

/* Latch the current size (call once before watching, so the first resize reads
 * as a change rather than initialization). */
static void wk_latch_size(void) {
    if (!wk_win_known) {
        wk_tty_control_state_t st;
        wk_tty_control_get(&st);
        wk_win_cols = st.cols;
        wk_win_rows = st.rows;
        wk_win_known = 1;
    }
}

/* Raise SIGWINCH if the size changed since last latched. Returns 1 if raised. */
static int wk_check_winch(void) {
    wk_tty_control_state_t st;
    wk_tty_control_get(&st);
    if (st.cols != wk_win_cols || st.rows != wk_win_rows) {
        wk_win_cols = st.cols;
        wk_win_rows = st.rows;
        raise(SIGWINCH);
        return 1;
    }
    return 0;
}

/* poll(2) built directly on wasi poll_oneoff. `timeout_ms` < 0 blocks. No
 * SIGWINCH handling here — callers add it around the wait. */
static int wk_poll_raw(struct pollfd *fds, nfds_t nfds, int timeout_ms) {
    if (nfds > WK_MAXFDS) {
        errno = EINVAL;
        return -1;
    }
    __wasi_subscription_t subs[WK_MAXFDS * 2 + 1];
    __wasi_event_t evs[WK_MAXFDS * 2 + 1];
    memset(subs, 0, sizeof(subs));
    size_t ns = 0;
    for (nfds_t i = 0; i < nfds; i++) {
        fds[i].revents = 0;
        if (fds[i].fd < 0)
            continue;
        if (fds[i].events & POLLIN) {
            subs[ns].userdata = ((uint64_t)i << 1) | 0;
            subs[ns].u.tag = __WASI_EVENTTYPE_FD_READ;
            subs[ns].u.u.fd_read.file_descriptor = (__wasi_fd_t)fds[i].fd;
            ns++;
        }
        if (fds[i].events & POLLOUT) {
            subs[ns].userdata = ((uint64_t)i << 1) | 1;
            subs[ns].u.tag = __WASI_EVENTTYPE_FD_WRITE;
            subs[ns].u.u.fd_write.file_descriptor = (__wasi_fd_t)fds[i].fd;
            ns++;
        }
    }
    if (timeout_ms >= 0) {
        subs[ns].userdata = ~(uint64_t)0;
        subs[ns].u.tag = __WASI_EVENTTYPE_CLOCK;
        subs[ns].u.u.clock.id = __WASI_CLOCKID_MONOTONIC;
        subs[ns].u.u.clock.timeout = (uint64_t)timeout_ms * 1000000ull;
        ns++;
    }
    __wasi_size_t nevents = 0;
    __wasi_errno_t e = __wasi_poll_oneoff(subs, evs, ns, &nevents);
    if (e != 0) {
        errno = (int)e;
        return -1;
    }
    int count = 0;
    for (size_t k = 0; k < nevents; k++) {
        if (evs[k].type == __WASI_EVENTTYPE_CLOCK)
            continue;
        size_t i = (size_t)(evs[k].userdata >> 1);
        if (i >= nfds)
            continue;
        if (evs[k].error)
            fds[i].revents |= POLLERR;
        else
            fds[i].revents |= (evs[k].userdata & 1) ? POLLOUT : POLLIN;
    }
    for (nfds_t i = 0; i < nfds; i++)
        if (fds[i].revents)
            count++;
    return count;
}

/* True if a poll/select set watches stdin for input. */
static int wk_watches_stdin(const struct pollfd *fds, nfds_t nfds) {
    for (nfds_t i = 0; i < nfds; i++)
        if (fds[i].fd == STDIN_FILENO && (fds[i].events & POLLIN))
            return 1;
    return 0;
}

int poll(struct pollfd *fds, nfds_t nfds, int timeout) {
    if (!wk_watches_stdin(fds, nfds))
        return wk_poll_raw(fds, nfds, timeout);
    wk_latch_size();
    int deadline_ms = timeout; /* remaining budget, or <0 for infinite */
    for (;;) {
        int slice = WK_WINCH_POLL_MS;
        if (deadline_ms >= 0 && deadline_ms < slice)
            slice = deadline_ms;
        int r = wk_poll_raw(fds, nfds, slice);
        if (r != 0)
            return r; /* an fd is ready, or error */
        /* Slice elapsed with nothing ready: deliver a pending resize, then
         * return 0 (timeout) so the app runs its resize path. */
        if (wk_check_winch())
            return 0;
        if (deadline_ms >= 0) {
            deadline_ms -= slice;
            if (deadline_ms <= 0)
                return 0; /* real timeout expired */
        }
    }
}

int select(int nfds, fd_set *readfds, fd_set *writefds, fd_set *exceptfds,
           struct timeval *timeout) {
    struct pollfd fds[WK_MAXFDS];
    nfds_t n = 0;
    for (int fd = 0; fd < nfds && n < WK_MAXFDS; fd++) {
        short ev = 0;
        if (readfds && FD_ISSET(fd, readfds))
            ev |= POLLIN;
        if (writefds && FD_ISSET(fd, writefds))
            ev |= POLLOUT;
        if (ev) {
            fds[n].fd = fd;
            fds[n].events = ev;
            fds[n].revents = 0;
            n++;
        }
    }
    int to = -1;
    if (timeout)
        to = (int)(timeout->tv_sec * 1000 + timeout->tv_usec / 1000);
    int r = poll(fds, n, to);
    if (r < 0)
        return -1;
    if (readfds)
        FD_ZERO(readfds);
    if (writefds)
        FD_ZERO(writefds);
    if (exceptfds)
        FD_ZERO(exceptfds);
    int count = 0;
    for (nfds_t i = 0; i < n; i++) {
        if (readfds && (fds[i].revents & (POLLIN | POLLERR | POLLHUP))) {
            FD_SET(fds[i].fd, readfds);
            count++;
        }
        if (writefds && (fds[i].revents & POLLOUT)) {
            FD_SET(fds[i].fd, writefds);
            count++;
        }
    }
    return count;
}

ssize_t read(int fd, void *buf, size_t count) {
    if (fd == STDIN_FILENO) {
        wk_latch_size();
        for (;;) {
            struct pollfd p = {.fd = STDIN_FILENO, .events = POLLIN, .revents = 0};
            int r = wk_poll_raw(&p, 1, WK_WINCH_POLL_MS);
            if (r > 0 && (p.revents & POLLIN))
                break; /* input available */
            if (r < 0)
                return -1;
            /* Slice elapsed: on a resize, deliver SIGWINCH and hand the app a
             * 0-byte read (its loop retries; the handler already redrew).
             * Otherwise keep waiting — no spurious 0 that could confuse a
             * mid-escape-sequence read. */
            if (wk_check_winch())
                return 0;
        }
    }
    __wasi_iovec_t iov = {.buf = buf, .buf_len = count};
    __wasi_size_t nread = 0;
    __wasi_errno_t e = __wasi_fd_read((__wasi_fd_t)fd, &iov, 1, &nread);
    if (e != 0) {
        errno = (int)e;
        return -1;
    }
    return (ssize_t)nread;
}

int ioctl(int fd, int request, ...) {
    (void)fd;
    if (request == TIOCGWINSZ) {
        va_list ap;
        va_start(ap, request);
        struct winsize *ws = va_arg(ap, struct winsize *);
        va_end(ap);
        if (!ws) {
            errno = EINVAL;
            return -1;
        }
        wk_tty_control_state_t st;
        wk_tty_control_get(&st);
        ws->ws_col = (unsigned short)st.cols;
        ws->ws_row = (unsigned short)st.rows;
        ws->ws_xpixel = 0;
        ws->ws_ypixel = 0;
        return 0;
    }
    errno = ENOTTY;
    return -1;
}

/* No output/input queues to drain or flush under WASI. */
int tcflush(int fd, int queue_selector) {
    (void)fd;
    (void)queue_selector;
    return 0;
}
int tcdrain(int fd) {
    (void)fd;
    return 0;
}
int tcflow(int fd, int action) {
    (void)fd;
    (void)action;
    return 0;
}

void cfmakeraw(struct termios *t) {
    if (!t)
        return;
    t->c_iflag &= ~(IGNBRK | BRKINT | PARMRK | ISTRIP | INLCR | IGNCR | ICRNL | IXON);
    t->c_oflag &= ~OPOST;
    t->c_lflag &= ~(ECHO | ECHONL | ICANON | ISIG | IEXTEN);
    t->c_cflag &= ~(CSIZE | PARENB);
    t->c_cflag |= CS8;
    t->c_cc[VMIN] = 1;
    t->c_cc[VTIME] = 0;
}

speed_t cfgetispeed(const struct termios *t) {
    return t ? t->c_ispeed : 0;
}
speed_t cfgetospeed(const struct termios *t) {
    return t ? t->c_ospeed : 0;
}
int cfsetispeed(struct termios *t, speed_t speed) {
    if (t)
        t->c_ispeed = speed;
    return 0;
}
int cfsetospeed(struct termios *t, speed_t speed) {
    if (t)
        t->c_ospeed = speed;
    return 0;
}
int cfsetspeed(struct termios *t, speed_t speed) {
    if (t) {
        t->c_ispeed = speed;
        t->c_ospeed = speed;
    }
    return 0;
}
