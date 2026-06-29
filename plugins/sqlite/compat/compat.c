/* WASI omits system(); the sqlite shell only calls it for the .system/.shell
 * dot-commands, which aren't meaningful in wk's sandbox. Stub it to fail so the
 * unmodified amalgamation links. */
int system(const char *cmd) {
    (void)cmd;
    return -1;
}
