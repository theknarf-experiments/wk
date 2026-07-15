/* A no-op termcap library for the wasi build.
 *
 * Vim links a termcap library only to read the host's termcap/terminfo DB.
 * A headless wasi terminal has none, so these stubs report "no entry" and Vim
 * falls back to its own builtin termcaps (builtin_xterm / builtin_ansi), which
 * is exactly what we want — the terminal is driven by wk's own VT engine. No
 * Vim source is modified; this only satisfies the tgetent() dependency. */

int tgetent(char *bp, const char *name) {
    (void)bp;
    (void)name;
    return -1; /* no termcap database available → Vim uses builtin termcaps */
}

int tgetflag(const char *id) {
    (void)id;
    return 0;
}

int tgetnum(const char *id) {
    (void)id;
    return -1;
}

char *tgetstr(const char *id, char **area) {
    (void)id;
    (void)area;
    return 0;
}

char *tgoto(const char *cap, int col, int row) {
    (void)cap;
    (void)col;
    (void)row;
    return (char *)"";
}

int tputs(const char *str, int affcnt, int (*putc_fn)(int)) {
    (void)affcnt;
    if (str && putc_fn)
        while (*str)
            putc_fn((unsigned char)*str++);
    return 0;
}
