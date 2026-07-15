/* <termios.h> for wasm32-wasi: WASI has no terminal-attributes interface, so
 * this bridges termios to wk's terminal. tcsetattr() detects raw mode (ICANON +
 * ECHO cleared) and emits wk's private raw-mode toggle, which the host
 * intercepts. Fuller than kilo's shim because Vim needs ECHOE (to select its
 * termios tty path, NEW_TTY_SYSTEM) and a few more flags/c_cc indices.
 * tcgetattr/tcsetattr are defined (non-static) in compat/wkos.c so they don't
 * clash with the prototypes Vim's generated osdef.h emits. No Vim edits. */
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

/* c_iflag */
#define IGNBRK  0x00000001
#define BRKINT  0x00000002
#define IGNPAR  0x00000004
#define PARMRK  0x00000008
#define INPCK   0x00000010
#define ISTRIP  0x00000020
#define INLCR   0x00000040
#define IGNCR   0x00000080
#define ICRNL   0x00000100
#define IXON    0x00000400
#define IXANY   0x00000800
#define IXOFF   0x00001000
/* c_oflag */
#define OPOST   0x00000001
#define ONLCR   0x00000004
/* c_cflag */
#define CSIZE   0x00000030
#define CS8     0x00000030
#define PARENB  0x00000100
/* c_lflag */
#define ISIG    0x00000001
#define ICANON  0x00000002
#define ECHO    0x00000008
#define ECHOE   0x00000010
#define ECHOK   0x00000020
#define ECHONL  0x00000040
#define IEXTEN  0x00008000

/* c_cc indices */
#define VINTR    0
#define VQUIT    1
#define VERASE   2
#define VKILL    3
#define VEOF     4
#define VTIME    5
#define VMIN     6
#define VSTART   8
#define VSTOP    9
#define VSUSP    10
#define VEOL     11

/* tcsetattr actions */
#define TCSANOW   0
#define TCSADRAIN 1
#define TCSAFLUSH 2

/* tcflush queues */
#define TCIFLUSH  0
#define TCOFLUSH  1
#define TCIOFLUSH 2

int tcgetattr(int fd, struct termios *t);
int tcsetattr(int fd, int actions, const struct termios *t);

static inline int tcflush(int fd, int queue) {
    (void)fd;
    (void)queue;
    return 0;
}
static inline speed_t cfgetispeed(const struct termios *t) {
    (void)t;
    return 0;
}
static inline speed_t cfgetospeed(const struct termios *t) {
    (void)t;
    return 0;
}
static inline int cfsetispeed(struct termios *t, speed_t s) {
    (void)t;
    (void)s;
    return 0;
}
static inline int cfsetospeed(struct termios *t, speed_t s) {
    (void)t;
    (void)s;
    return 0;
}

#endif
