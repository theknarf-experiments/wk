/* A minimal termcap library for the wasi build.
 *
 * Vim's Unix build always links a termcap library (HAVE_TGETENT). A headless
 * wasi terminal has no termcap/terminfo database, so tgetent() reports "no
 * entry" and Vim falls back to its own builtin termcaps (builtin_xterm /
 * builtin_ansi) — exactly what we want, since wk's own VT engine renders the
 * output. No Vim source is modified.
 *
 * BUT the builtin capability strings still contain termcap parameter codes
 * (`\e[%i%d;%dH` for cursor motion, etc.), and Vim formats them by calling the
 * library's tgoto(). A stub that returned "" made every cursor-addressing write
 * land at the same spot (the intro screen piled all its lines on one row). So
 * tgoto() below is a REAL termcap formatter; only the database lookups are
 * stubbed. Since TERMINFO is off, the builtin caps use termcap-style codes,
 * which is precisely what tgoto() understands. */

int tgetent(char *bp, char *name) {
    (void)bp;
    (void)name;
    return -1; /* no termcap database available -> Vim uses builtin termcaps */
}

int tgetflag(char *id) {
    (void)id;
    return 0;
}

int tgetnum(char *id) {
    (void)id;
    return -1;
}

char *tgetstr(char *id, char **area) {
    (void)id;
    (void)area;
    return 0;
}

/* Format a termcap cursor-motion (or similar) capability string.
 *
 * Called as tgoto(cap, col, line): the FIRST parameter code in `cap` consumes
 * `line`, the second consumes `col` (the classic termcap convention Vim's own
 * fallback tgoto in term.c mirrors). Handles the codes the builtin caps use:
 *   %d  emit the current arg as decimal      %i  increment both args (1-based)
 *   %2 %3  emit zero-padded to width 2 / 3    %+x emit (arg + x) as a raw byte
 *   %.  emit the arg as a raw byte            %r  swap the two args
 *   %>xy add y to the arg if it exceeds x     %%  a literal '%'
 * Anything else is copied through verbatim. */
char *tgoto(char *cap, int col, int line) {
    static char buf[32];
    char *s = buf;
    char *end = buf + sizeof(buf) - 1;
    int args[2];
    int ai = 0;
    int add = 0;

    if (cap == 0)
        return (char *)"";

    args[0] = line;
    args[1] = col;

    while (*cap && s < end) {
        if (*cap != '%') {
            *s++ = *cap++;
            continue;
        }
        cap++;
        switch (*cap++) {
        case 'd': {
            int v = args[ai < 2 ? ai : 1] + add;
            ai++;
            add = 0;
            char tmp[12];
            int n = 0;
            if (v < 0)
                v = 0;
            do {
                tmp[n++] = (char)('0' + v % 10);
                v /= 10;
            } while (v && n < (int)sizeof(tmp));
            while (n > 0 && s < end)
                *s++ = tmp[--n];
            break;
        }
        case '2':
        case '3': {
            int width = cap[-1] - '0';
            int v = args[ai < 2 ? ai : 1] + add;
            ai++;
            add = 0;
            if (v < 0)
                v = 0;
            char tmp[12];
            int n = 0;
            do {
                tmp[n++] = (char)('0' + v % 10);
                v /= 10;
            } while (v && n < (int)sizeof(tmp));
            while (n < width && n < (int)sizeof(tmp))
                tmp[n++] = '0';
            while (n > 0 && s < end)
                *s++ = tmp[--n];
            break;
        }
        case '.': {
            int v = args[ai < 2 ? ai : 1] + add;
            ai++;
            add = 0;
            *s++ = (char)v;
            break;
        }
        case '+': {
            int v = args[ai < 2 ? ai : 1] + (unsigned char)*cap++;
            ai++;
            add = 0;
            *s++ = (char)v;
            break;
        }
        case 'i':
            args[0]++;
            args[1]++;
            break;
        case 'r': {
            int t = args[0];
            args[0] = args[1];
            args[1] = t;
            break;
        }
        case '>':
            /* %>xy: if the current arg > x, add y to it. */
            if (args[ai < 2 ? ai : 1] > (unsigned char)cap[0])
                add += (unsigned char)cap[1];
            cap += 2;
            break;
        case '%':
            *s++ = '%';
            break;
        default:
            /* Unknown code: emit it literally so nothing is silently dropped. */
            *s++ = cap[-1];
            break;
        }
    }
    *s = '\0';
    return buf;
}

int tputs(char *str, int affcnt, int (*putc_fn)(int)) {
    (void)affcnt;
    if (str && putc_fn)
        while (*str)
            putc_fn((unsigned char)*str++);
    return 0;
}
