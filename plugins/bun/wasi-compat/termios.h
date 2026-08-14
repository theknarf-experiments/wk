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
#ifdef __cplusplus
extern "C" {
#endif
static inline int tcgetattr(int fd, struct termios *t) { (void)fd; (void)t; return -1; }
static inline int tcsetattr(int fd, int a, const struct termios *t) { (void)fd; (void)a; (void)t; return -1; }
static inline speed_t cfgetispeed(const struct termios *t) { (void)t; return 0; }
static inline speed_t cfgetospeed(const struct termios *t) { (void)t; return 0; }
static inline int cfsetispeed(struct termios *t, speed_t s) { (void)t; (void)s; return -1; }
static inline int cfsetospeed(struct termios *t, speed_t s) { (void)t; (void)s; return -1; }
#ifdef __cplusplus
}
#endif
#endif
