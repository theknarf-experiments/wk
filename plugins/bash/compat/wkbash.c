/* Running external commands from bash, on a platform with no fork/exec.
 *
 * bash normally forks and then execve()s in the child. WASI has no fork, so
 * the patched execute_disk_command() calls wk_bash_run() instead: it runs the
 * program to completion through wk's `wk:exec` capability and reports the
 * status the shell would have waited for. That is exec semantics minus the
 * fork, which is exactly what the shell needs for a plain command.
 *
 * Command resolution is bash's own: it searches PATH and hands us what it
 * found. Nothing special is needed for multicall binaries — wk's filesystem
 * has real symlinks, so `/bin/ls -> coreutils.wasm` resolves like it does
 * anywhere else, and argv[0] stays "ls", which is what coreutils dispatches
 * on.
 *
 * The child's output is captured, so it is written to bash's stdout/stderr
 * here. `>` redirection still cannot work: saving and restoring a descriptor
 * needs dup(), which WASI does not have.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "wkexec.h"

static void write_all(int fd, const char *buf, size_t len) {
    size_t off = 0;
    while (off < len) {
        ssize_t n = write(fd, buf + off, len - off);
        if (n <= 0)
            break;
        off += (size_t)n;
    }
}

/* Run an external command. Returns its exit status, or -1 if it could not be
 * run at all (so the caller can fall through to bash's own error paths). */
int wk_bash_run(const char *command, char **argv, const char *typed) {
    if (!command || !*command)
        return -1; /* not on PATH: let bash report it */

    wk_result r;
    int rc = wk_run(command, (const char *const *)argv, NULL, 0, &r);
    if (rc != 0 || r.error) {
        if (r.error)
            fprintf(stderr, "%s: %s\n", typed ? typed : command, r.error);
        wk_result_free(&r);
        return 126; /* found but not executable, as a shell reports it */
    }

    write_all(STDOUT_FILENO, r.stdout_data, r.stdout_len);
    write_all(STDERR_FILENO, r.stderr_data, r.stderr_len);
    int status = r.exit_code;
    wk_result_free(&r);
    return status;
}
