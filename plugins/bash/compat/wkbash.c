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
#include "exec_host.h"

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

    /* Standard input reaches the child one of three ways. A pipe — a here
     * document, or `cmd < <(...)` — is handed over as the pipe itself, so it
     * streams and so a document larger than the pipe's buffer cannot deadlock.
     * A regular file is read here (see stdin_bytes). Anything else, a terminal
     * above all, gives the child nothing. */
    wk_exec_process_borrow_pipe_t inpipe;
    if (wk_pipe_of_fd(STDIN_FILENO, &inpipe)) {
        wk_stdio in_io = {0};
        in_io.pipe_borrow = &inpipe;
        wk_child child;
        char *err = NULL;
        if (wk_spawn(command, (const char *const *)argv, &in_io, NULL, NULL,
                     &child, &err)) {
            fprintf(stderr, "%s: %s\n", typed ? typed : command,
                    err ? err : "cannot run");
            free(err);
            return 126;
        }
        wk_result pr;
        int status = 126;
        if (wk_wait(child, &pr) == 0) {
            write_all(STDOUT_FILENO, pr.stdout_data, pr.stdout_len);
            write_all(STDERR_FILENO, pr.stderr_data, pr.stderr_len);
            status = pr.exit_code;
        } else if (pr.error) {
            fprintf(stderr, "%s: %s\n", typed ? typed : command, pr.error);
        }
        wk_result_free(&pr);
        return status;
    }

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


/* --- command substitution: a real subshell, in another instance ------------
 *
 * `$(...)' is a subshell whose output the shell reads back, and there is no
 * fork to make one with. Running it inside this shell is not an option: the
 * expansion that asked for it is still walking its word list, and re-entering
 * the executor recycles the cached WORD_DESCs it is holding — bash's object
 * cache scrambles freed objects with 0xdf, which is exactly the address such a
 * shell traps on.
 *
 * So run it in a *second bash*, started through wk:exec with its stdout on a
 * pipe. That is a genuine subshell — its own instance, its own memory, its own
 * caches — so the aliasing cannot arise, and side effects correctly do not
 * leak back, which is what a subshell means.
 *
 * What it does not get is this shell's unexported state: functions and
 * variables that were never exported are not visible to another instance.
 * The exported environment is passed along.
 */
#define WK_MAX_COMSUB 8
static wk_child comsub_child[WK_MAX_COMSUB];
static int comsub_depth;

/* Start `shell -c script' with its stdout on a pipe. Returns a descriptor to
 * read the output from, or -1; `*tok' identifies the child for the wait. */
int wk_bash_comsub_start(const char *shell, const char *script, char **envp,
                         int *tok) {
    int fds[2];

    if (comsub_depth >= WK_MAX_COMSUB)
        return -1;
    if (!shell || !*shell || pipe(fds) != 0)
        return -1;

    wk_exec_process_borrow_pipe_t wp;
    if (!wk_pipe_of_fd(fds[1], &wp)) {
        close(fds[0]);
        close(fds[1]);
        return -1;
    }

    /* The child's environment is this shell's exported one. */
    size_t nenv = 0;
    while (envp && envp[nenv])
        nenv++;
    exec_host_list_tuple2_string_string_t wenv = {NULL, 0};
    if (nenv) {
        wenv.ptr = malloc(nenv * sizeof *wenv.ptr);
        if (!wenv.ptr) {
            close(fds[0]);
            close(fds[1]);
            return -1;
        }
        for (size_t i = 0; i < nenv; i++) {
            const char *eq = strchr(envp[i], '=');
            size_t nlen = eq ? (size_t)(eq - envp[i]) : strlen(envp[i]);
            exec_host_string_t k, v;
            k.ptr = (uint8_t *)envp[i];
            k.len = nlen;
            v.ptr = (uint8_t *)(eq ? eq + 1 : "");
            v.len = eq ? strlen(eq + 1) : 0;
            wenv.ptr[i].f0 = k;
            wenv.ptr[i].f1 = v;
        }
        wenv.len = nenv;
    }

    exec_host_string_t wpath;
    exec_host_string_set(&wpath, shell);
    exec_host_string_t wargs_buf[3];
    /* argv[0] is the shell's own path, not "bash": a substitution inside the
     * child has to start a third instance, and it finds itself by this. */
    exec_host_string_set(&wargs_buf[0], (char *)shell);
    exec_host_string_set(&wargs_buf[1], "-c");
    exec_host_string_set(&wargs_buf[2], (char *)script);
    exec_host_list_string_t wargs = {wargs_buf, 3};

    wk_exec_process_stdin_from_t win;
    win.tag = WK_EXEC_PROCESS_STDIN_FROM_EMPTY;
    wk_exec_process_stdout_to_t wout, werr;
    wout.tag = WK_EXEC_PROCESS_STDOUT_TO_PIPE_END;
    wout.val.pipe_end = wp;
    werr.tag = WK_EXEC_PROCESS_STDOUT_TO_CAPTURE;

    wk_exec_process_own_child_t child;
    exec_host_string_t err;
    bool ok = wk_exec_process_spawn(&wpath, &wargs, &wenv, &win, &wout, &werr,
                                    &child, &err);
    free(wenv.ptr);
    if (!ok) {
        exec_host_string_free(&err);
        close(fds[0]);
        close(fds[1]);
        return -1;
    }

    /* Let go of this shell's copy of the write end, or the reader below would
     * wait for an end-of-file that only the child's exit can no longer give. */
    close(fds[1]);

    comsub_child[comsub_depth].h = child.__handle;
    *tok = comsub_depth++;
    return fds[0];
}

/* Collect the child started above; returns its exit status. */
int wk_bash_comsub_wait(int tok) {
    wk_result r;
    int status = 127;

    if (tok < 0 || tok >= comsub_depth)
        return status;
    if (wk_wait(comsub_child[tok], &r) == 0) {
        /* stderr was captured rather than piped, so pass it through. */
        write_all(STDERR_FILENO, r.stderr_data, r.stderr_len);
        status = r.exit_code;
    }
    wk_result_free(&r);
    if (tok == comsub_depth - 1)
        comsub_depth--;
    return status;
}

/* --- subshells: `( ... )` groups and compound pipeline stages --------------
 *
 * A subshell is a child the shell would fork; there is none here. Run it as a
 * second bash through wk:exec, like command substitution above, but with its
 * stdio wired where a subshell's belongs. `script' is its text (from
 * make_command_string); `in_fd'/`out_fd' are the pipeline pipe endpoints, or -1
 * for a standalone `( ... )`. A stage that feeds another (out_fd set) streams
 * into it and is left running — its status is not the pipeline's; anything else
 * is captured, replayed to this shell's stdout/stderr, and its real status
 * returned, exactly as the forked path would have waited for it.
 *
 * Like command substitution, the child inherits only the exported environment —
 * unexported variables and functions are not visible to another instance. */
int wk_bash_subshell(const char *shell, const char *script, char **envp,
                     int in_fd, int out_fd) {
    if (!shell || !*shell || !script)
        return 126;

    size_t nenv = 0;
    while (envp && envp[nenv])
        nenv++;
    exec_host_list_tuple2_string_string_t wenv = {NULL, 0};
    if (nenv) {
        wenv.ptr = malloc(nenv * sizeof *wenv.ptr);
        if (!wenv.ptr)
            return 126;
        for (size_t i = 0; i < nenv; i++) {
            const char *eq = strchr(envp[i], '=');
            size_t nlen = eq ? (size_t)(eq - envp[i]) : strlen(envp[i]);
            exec_host_string_t k, v;
            k.ptr = (uint8_t *)envp[i];
            k.len = nlen;
            v.ptr = (uint8_t *)(eq ? eq + 1 : "");
            v.len = eq ? strlen(eq + 1) : 0;
            wenv.ptr[i].f0 = k;
            wenv.ptr[i].f1 = v;
        }
        wenv.len = nenv;
    }

    exec_host_string_t wpath;
    exec_host_string_set(&wpath, shell);
    exec_host_string_t wargs_buf[3];
    exec_host_string_set(&wargs_buf[0], (char *)shell);
    exec_host_string_set(&wargs_buf[1], "-c");
    exec_host_string_set(&wargs_buf[2], (char *)script);
    exec_host_list_string_t wargs = {wargs_buf, 3};

    /* stdin: a pipeline stage reads the pipe it was handed; a standalone
     * subshell reads this shell's own stdin — a pipe streams, a regular file is
     * read here, a terminal gives nothing (the three ways wk_bash_run picks). */
    wk_exec_process_stdin_from_t win;
    wk_exec_process_borrow_pipe_t inp;
    char *inbytes = NULL;
    size_t inbytes_len = 0;
    int wire_in = in_fd >= 0 ? in_fd : STDIN_FILENO;
    if (wk_pipe_of_fd(wire_in, &inp)) {
        win.tag = WK_EXEC_PROCESS_STDIN_FROM_PIPE_END;
        win.val.pipe_end = inp;
    } else if (in_fd < 0 && (inbytes = stdin_bytes(&inbytes_len)) != NULL) {
        win.tag = WK_EXEC_PROCESS_STDIN_FROM_BYTES;
        win.val.bytes.ptr = (uint8_t *)inbytes;
        win.val.bytes.len = inbytes_len;
    } else {
        win.tag = WK_EXEC_PROCESS_STDIN_FROM_EMPTY;
    }

    /* stdout: stream into the next stage if there is one, else capture. */
    wk_exec_process_stdout_to_t wout, werr;
    wk_exec_process_borrow_pipe_t outp;
    int streaming = 0;
    if (out_fd >= 0 && wk_pipe_of_fd(out_fd, &outp)) {
        wout.tag = WK_EXEC_PROCESS_STDOUT_TO_PIPE_END;
        wout.val.pipe_end = outp;
        streaming = 1;
    } else {
        wout.tag = WK_EXEC_PROCESS_STDOUT_TO_CAPTURE;
    }
    werr.tag = WK_EXEC_PROCESS_STDOUT_TO_CAPTURE;

    /* The shell's own buffered output goes out before the child's, which is
     * written straight to the descriptors below. */
    fflush(stdout);
    fflush(stderr);

    wk_exec_process_own_child_t ochild;
    exec_host_string_t err = {NULL, 0};
    bool ok = wk_exec_process_spawn(&wpath, &wargs, &wenv, &win, &wout, &werr,
                                    &ochild, &err);
    free(wenv.ptr);
    free(inbytes);
    if (!ok) {
        fprintf(stderr, "%s\n", err.len ? (char *)err.ptr : "cannot start subshell");
        exec_host_string_free(&err);
        return 126;
    }

    wk_child child;
    child.h = ochild.__handle;

    if (streaming) {
        /* Left running like any non-final pipeline stage — released, not waited
         * for (waiting would deadlock a reader that leaves early). */
        if (npending < WK_MAX_STAGES)
            pending[npending++] = child;
        else
            wk_child_detach(child);
        return 0; /* EXECUTION_SUCCESS — a mid-pipeline stage's status is unused */
    }

    wk_result r;
    int status = 126;
    if (wk_wait(child, &r) == 0) {
        write_all(STDOUT_FILENO, r.stdout_data, r.stdout_len);
        write_all(STDERR_FILENO, r.stderr_data, r.stderr_len);
        status = r.exit_code;
    } else if (r.error) {
        fprintf(stderr, "%s\n", r.error);
    }
    wk_result_free(&r);
    return status;
}
