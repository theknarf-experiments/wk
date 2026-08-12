/* A small C API over wk's `wk:exec` capability.
 *
 * WASI has no fork/exec: a program in the sandbox cannot start another. wk
 * runs components itself, so it offers "run a program" as a capability — the
 * child is read from the caller's own filesystem, shares that filesystem, and
 * inherits the caller's sandbox and capability token.
 *
 * This header keeps the generated bindings (gen/exec_host.h) out of callers:
 * an unmodified C program only needs `wk_run()`, which looks like a blocking
 * `posix_spawn` + `waitpid` pair.
 */
#ifndef _WK_EXEC_H
#define _WK_EXEC_H

#include <stddef.h>

/* What a finished program left behind. `stdout_data`/`stderr_data` are
 * malloc'd and owned by the caller; free with wk_result_free(). */
typedef struct {
    int exit_code;
    char *stdout_data;
    size_t stdout_len;
    char *stderr_data;
    size_t stderr_len;
    /* NULL on success; otherwise why the program could not be run at all
     * (no such file, not a component, exec not permitted, nesting too deep). */
    char *error;
} wk_result;

/* Run `path` (a wasm component in this node's filesystem) to completion.
 * `argv` is NULL-terminated and is the child's argv *in full*, including
 * argv[0] — execve semantics. That matters for multicall binaries: GNU
 * coreutils dispatches on argv[0], so running /bin/coreutils.wasm with
 * argv[0]="ls" is how one binary provides a hundred commands.
 * `stdin_data`/`stdin_len` feed the child's stdin.
 *
 * Returns 0 if the program ran (check `exit_code`), -1 if it could not be run
 * (check `error`). Blocks until the child exits — these are exec semantics
 * without the fork, so a caller builds a pipeline by feeding one program's
 * stdout into the next one's stdin. */
int wk_run(const char *path, const char *const *argv,
           const char *stdin_data, size_t stdin_len, wk_result *out);

void wk_result_free(wk_result *r);

#endif
