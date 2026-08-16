/* Honor a working directory handed down through the environment.
 *
 * wk:exec cannot set a child's initial cwd — it is wasi-libc state ("/" until
 * something chdir()s), not anything the host can inject. So a launcher that
 * wants the child to start somewhere else passes __WK_EXEC_CWD, and this
 * constructor chdir()s into it before main runs. That is how Bun.spawn's `cwd`
 * and a shell's `cd` before an external command reach the program that runs. */
#include <stdlib.h>
#include <unistd.h>

__attribute__((constructor)) static void wk_exec_chdir(void) {
    const char *d = getenv("__WK_EXEC_CWD");
    if (d && *d)
        (void)chdir(d);
}
