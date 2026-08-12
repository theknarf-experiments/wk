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
 * here — which is also how redirection works: the patched caller applies the
 * command's redirections around this call, so fd 1 already points wherever
 * `>` sent it by the time we write.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include "wkexec.h"
#include "wkpipe.h"

static void write_all(int fd, const char *buf, size_t len) {
    size_t off = 0;
    while (off < len) {
        ssize_t n = write(fd, buf + off, len - off);
        if (n <= 0)
            break;
        off += (size_t)n;
    }
}

/* wk:exec hands the child a buffer of stdin rather than a descriptor, so a
 * `< file` redirection — which the caller has already applied to fd 0 — has to
 * be read here and passed along.
 *
 * Only for a regular file. A terminal has no end: slurping it would hang the
 * shell waiting for the input the child was supposed to consume. Reading from
 * the current offset and leaving it at EOF is what the child would have done
 * to the shared file description anyway. */
static char *stdin_bytes(size_t *len) {
    struct stat st;
    char *buf = NULL;
    size_t cap = 0, used = 0;

    *len = 0;
    if (fstat(STDIN_FILENO, &st) != 0 || !S_ISREG(st.st_mode))
        return NULL;

    for (;;) {
        if (used == cap) {
            size_t want = cap ? cap * 2 : 8192;
            char *grown = realloc(buf, want);
            if (!grown) {
                free(buf);
                return NULL;
            }
            buf = grown;
            cap = want;
        }
        ssize_t n = read(STDIN_FILENO, buf + used, cap - used);
        if (n < 0) {
            free(buf);
            return NULL;
        }
        if (n == 0)
            break;
        used += (size_t)n;
    }
    *len = used;
    return buf;
}

/* Stages of a pipeline that were started but not waited for.
 *
 * A pipeline's stages have to run at the same time — the writer fills the pipe
 * and stops until the reader drains it — so every stage but the last is
 * spawned and left running. The shell reports the *last* stage's status, so
 * these are only let go of, never waited for.
 *
 * Waiting would deadlock the common case: in `seq 1 200000 | head -1' the
 * reader leaves early, and seq is then blocked on a full pipe until the last
 * reader goes — but the shell still holds its own copy of the read descriptor
 * and does not close it until after this returns. Detaching lets seq fail its
 * next write and exit on its own, which is what a shell's SIGPIPE does. */
#define WK_MAX_STAGES 32
static wk_child pending[WK_MAX_STAGES];
static int npending;

static void release_pending(void) {
    for (int i = 0; i < npending; i++)
        wk_child_detach(pending[i]);
    npending = 0;
}

/* Run one stage of a pipeline. `in_fd`/`out_fd` are the shell's pipe
 * descriptors, or -1; wk_pipe_of_fd turns them back into the pipe itself so
 * the child's stdio *is* the pipe rather than a copy of its bytes.
 *
 * A stage that feeds another must not be waited for here, or the pipeline
 * deadlocks the moment the pipe fills. So only the last stage — the one with
 * nothing to write to — is waited for, and its status is the pipeline's, which
 * is what the shell reports anyway. */
int wk_bash_run_stage(const char *command, char **argv, const char *typed,
                      int in_fd, int out_fd) {
    if (!command || !*command)
        return -1;

    wk_exec_process_borrow_pipe_t inp, outp;
    bool has_in = in_fd >= 0 && wk_pipe_of_fd(in_fd, &inp);
    bool has_out = out_fd >= 0 && wk_pipe_of_fd(out_fd, &outp);

    wk_stdio in_io = {0}, out_io = {0};
    if (has_in)
        in_io.pipe_borrow = &inp;
    if (has_out)
        out_io.pipe_borrow = &outp;

    wk_child child;
    char *err = NULL;
    if (wk_spawn(command, (const char *const *)argv, has_in ? &in_io : NULL,
                 has_out ? &out_io : NULL, NULL, &child, &err)) {
        fprintf(stderr, "%s: %s\n", typed ? typed : command, err ? err : "cannot run");
        free(err);
        return 126;
    }

    if (has_out) {
        /* Feeds another stage: leave it running. */
        if (npending < WK_MAX_STAGES)
            pending[npending++] = child;
        return 0;
    }

    wk_result r;
    int status = 126;
    if (wk_wait(child, &r) == 0) {
        write_all(STDOUT_FILENO, r.stdout_data, r.stdout_len);
        write_all(STDERR_FILENO, r.stderr_data, r.stderr_len);
        status = r.exit_code;
    } else if (r.error) {
        fprintf(stderr, "%s: %s\n", typed ? typed : command, r.error);
    }
    wk_result_free(&r);
    release_pending();
    return status;
}

/* Run an external command. Returns its exit status, or -1 if it could not be
 * run at all (so the caller can fall through to bash's own error paths). */
int wk_bash_run(const char *command, char **argv, const char *typed) {
    if (!command || !*command)
        return -1; /* not on PATH: let bash report it */

    size_t in_len;
    char *in = stdin_bytes(&in_len);
    wk_result r;
    int rc = wk_run(command, (const char *const *)argv, in, in_len, &r);
    free(in);
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
