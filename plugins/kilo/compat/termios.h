/* Minimal <termios.h> for wasm32-wasi: wasi-libc has no termios because WASI
 * has no terminal-attributes interface yet. This bridges it to wk's terminal —
 * tcsetattr() detects raw mode (canonical + echo turned off) and emits wk's
 * private raw-mode toggle, which the host intercepts. Reusable by any termios
 * app (the same shim is what a vim port would use). No kilo.c changes needed. */
#ifndef _WK_COMPAT_TERMIOS_H
#define _WK_COMPAT_TERMIOS_H

#include <unistd.h>

typedef unsigned int tcflag_t;
typedef unsigned char cc_t;
typedef unsigned int speed_t;

#define NCCS 32
struct termios {
    tcflag_t c_iflag, c_oflag, c_cflag, c_lflag;
    cc_t c_cc[NCCS];
};

/* Input flags */
#define BRKINT  0x0001
#define ICRNL   0x0002
#define INPCK   0x0004
#define ISTRIP  0x0008
#define IXON    0x0010
/* Output flags */
#define OPOST   0x0001
/* Control flags */
#define CS8     0x0030
/* Local flags */
#define ECHO    0x0001
#define ICANON  0x0002
#define IEXTEN  0x0004
#define ISIG    0x0008

/* c_cc indices */
#define VMIN    6
#define VTIME   5

/* tcsetattr actions */
#define TCSANOW   0
#define TCSADRAIN 1
#define TCSAFLUSH 2

static inline int tcgetattr(int fd, struct termios *t) {
    (void)fd;
    if (!t) return -1;
    t->c_iflag = BRKINT | ICRNL | INPCK | ISTRIP | IXON;
    t->c_oflag = OPOST;
    t->c_cflag = CS8;
    t->c_lflag = ECHO | ICANON | IEXTEN | ISIG; /* cooked */
    for (int i = 0; i < NCCS; i++) t->c_cc[i] = 0;
    return 0;
}

static inline int tcsetattr(int fd, int actions, const struct termios *t) {
    (void)fd; (void)actions;
    if (!t) return -1;
    if (!(t->c_lflag & (ICANON | ECHO)))
        (void)write(STDOUT_FILENO, "\x1b[?7777h", 8); /* enter raw */
    else
        (void)write(STDOUT_FILENO, "\x1b[?7777l", 8); /* leave raw */
    return 0;
}

#endif
