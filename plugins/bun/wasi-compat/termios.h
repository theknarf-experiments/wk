// Minimal termios stub for wasi (no TTY layer; TTY code paths are inert on wasm).
#ifndef _WASI_COMPAT_TERMIOS_H
#define _WASI_COMPAT_TERMIOS_H
#include <stdint.h>
typedef unsigned int tcflag_t;
typedef unsigned char cc_t;
typedef unsigned int speed_t;
#define NCCS 32
struct termios {
    tcflag_t c_iflag, c_oflag, c_cflag, c_lflag;
    cc_t c_cc[NCCS];
    speed_t c_ispeed, c_ospeed;
};
struct winsize { unsigned short ws_row, ws_col, ws_xpixel, ws_ypixel; };
#define TCSANOW 0
#define TCSADRAIN 1
#define TCSAFLUSH 2
#define ICANON 0000002
#define ECHO   0000010
#define VMIN 6
#define VTIME 5
#define TIOCGWINSZ 0x5413
#define IGNBRK 0000001
#define BRKINT 0000002
#define IGNPAR 0000004
#define PARMRK 0000010
#define INPCK  0000020
#define ISTRIP 0000040
#define INLCR  0000100
#define IGNCR  0000200
#define ICRNL  0000400
#define IXON   0002000
#define IXANY  0004000
#define IXOFF  0010000
#define OPOST  0000001
#define ONLCR  0000004
#define CSIZE  0000060
#define CS8    0000060
#define PARENB 0000400
#define ISIG   0000001
#define IEXTEN 0100000
#define ECHOE  0000020
#define ECHOK  0000040
#define ECHONL 0000100
#define NOFLSH 0000200
#ifdef __cplusplus
extern "C" {
#endif
static inline int tcgetattr(int fd, struct termios *t) { (void)fd; (void)t; return -1; }
static inline int tcsetattr(int fd, int a, const struct termios *t) { (void)fd; (void)a; (void)t; return -1; }
static inline speed_t cfgetispeed(const struct termios *t) { (void)t; return 0; }
static inline speed_t cfgetospeed(const struct termios *t) { (void)t; return 0; }
static inline int cfsetispeed(struct termios *t, speed_t s) { (void)t; (void)s; return -1; }
static inline int cfsetospeed(struct termios *t, speed_t s) { (void)t; (void)s; return -1; }
static inline void cfmakeraw(struct termios *t) { (void)t; }
static inline int ttyname_r(int fd, char *buf, unsigned long len) { (void)fd; if (buf && len) buf[0] = 0; return 0; }
#ifdef __cplusplus
}
#endif
#endif
