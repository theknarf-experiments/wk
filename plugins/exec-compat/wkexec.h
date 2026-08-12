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
#include <stdint.h>

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

/* ---------------------------------------------------------------------------
 * Pipelines: spawn instead of run.
 *
 * wk_run() cannot express `a | b`, because the producer has to finish before
 * the consumer starts — its stdin *is* the producer's collected output. So
 * `seq 1 200000 | head -1` would run to 200000, and `yes | head` would never
 * finish. wk_spawn() starts a program and returns immediately, so two of them
 * can share a pipe and the bytes move as they are written.
 *
 * Unlike POSIX there is no parent copy of the pipe to remember to close: a
 * child gets its own counted end, and end-of-file is when the last one exits.
 */

/* A pipe, and a running program. Both are wk resource handles. */
typedef struct { int32_t h; } wk_pipe;
typedef struct { int32_t h; } wk_child;

wk_pipe wk_pipe_new(void);
void wk_pipe_free(wk_pipe p);

/* Where a spawned child's stdio goes. Pass NULL for `pipe` to mean
 * "capture it" (readable from wk_wait) for output, or "no input" for stdin. */
typedef struct {
    const wk_pipe *pipe;   /* the pipe, or NULL */
    const char *bytes;     /* stdin only: fixed input, used when pipe is NULL */
    size_t len;
} wk_stdio;

/* Start `path` without waiting. argv is NULL-terminated and complete, as for
 * wk_run. Returns 0 and fills `out`, or -1 and sets `*error` (free it). */
int wk_spawn(const char *path, const char *const *argv,
             const wk_stdio *in, const wk_stdio *out_io, const wk_stdio *err_io,
             wk_child *out, char **error);

/* Block until `child` exits, reporting its status and any *captured* output
 * (a stream sent to a pipe has already gone to whoever read it). Consumes the
 * handle. Returns 0 if it ran, -1 otherwise (check `out->error`). */
int wk_wait(wk_child child, wk_result *out);

#endif
