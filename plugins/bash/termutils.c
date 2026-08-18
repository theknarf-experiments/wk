/* clear(1) / reset(1) for the wk-shell image — the two terminal utilities
 * every shell user's fingers expect, which ship with ncurses everywhere
 * else. One multicall binary dispatching on argv[0], installed as symlinks
 * exactly like the coreutils farm. No termcap lookup: wk's terminal is an
 * xterm-family VT emulator, so the standard sequences are the right ones. */
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    (void)argc;
    const char *name = argv[0] ? argv[0] : "clear";
    const char *slash = strrchr(name, '/');
    if (slash)
        name = slash + 1;
    const char *seq;
    if (strncmp(name, "reset", 5) == 0) {
        /* RIS: full terminal reset — modes, charsets, and the screen. */
        seq = "\033c";
    } else {
        /* Home, clear screen, clear scrollback — what ncurses clear sends. */
        seq = "\033[H\033[2J\033[3J";
    }
    write(1, seq, strlen(seq));
    return 0;
}
