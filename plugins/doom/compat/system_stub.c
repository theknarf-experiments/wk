/* wasi-libc declares system() in stdlib.h but (having no processes) never
 * defines it. doomgeneric's only caller is i_system.c's zenity error box
 * (I_Error popup): returning -1 means "zenity unavailable", which cleanly
 * skips the popup and leaves the error on stderr. */
int system(const char *command)
{
    (void)command;
    return -1;
}
