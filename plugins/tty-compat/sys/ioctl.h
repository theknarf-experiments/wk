/* <sys/ioctl.h> for wasm32-wasi: enough for terminal apps to query the window
 * size. TIOCGWINSZ is served from wk's `wk:tty/control` (see termios.c), so the
 * size is accurate instead of relying on an ESC[6n cursor-report dance. Other
 * requests return -1/ENOTTY. */
#ifndef _WK_TTY_SYS_IOCTL_H
#define _WK_TTY_SYS_IOCTL_H

#if !defined(__DEFINED_struct_winsize)
struct winsize {
    unsigned short ws_row;
    unsigned short ws_col;
    unsigned short ws_xpixel;
    unsigned short ws_ypixel;
};
#define __DEFINED_struct_winsize
#endif

#define TIOCGWINSZ 0x5413
#define TIOCSWINSZ 0x5414

int ioctl(int fd, int request, ...);

#endif /* _WK_TTY_SYS_IOCTL_H */
