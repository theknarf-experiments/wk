/* Augment wasi-libc's <sys/ioctl.h>: it declares ioctl() (which returns -1 for
 * an unknown request) but lacks TIOCGWINSZ / struct winsize. With those defined,
 * kilo's ioctl(TIOCGWINSZ) fails cleanly and it falls back to the ESC[6n
 * cursor-position query — which wk's terminal answers. So window size needs no
 * kilo changes and no host change beyond the DSR reply we already send. */
#ifndef _WK_COMPAT_SYS_IOCTL_H
#define _WK_COMPAT_SYS_IOCTL_H
#include_next <sys/ioctl.h>
#ifndef TIOCGWINSZ
#define TIOCGWINSZ 0x5413
struct winsize {
    unsigned short ws_row, ws_col, ws_xpixel, ws_ypixel;
};
#endif
#endif
