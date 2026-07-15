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
#include <stdarg.h>
#include <sys/ioctl.h>
#include <termios.h>

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
