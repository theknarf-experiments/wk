/* Implementation of the wk_run() wrapper: marshals to the generated
 * `wk:exec` bindings and copies the results into plain C memory. See
 * wkexec.h. */
#include "wkexec.h"

#include <stdlib.h>
#include <string.h>

#include "exec_host.h"

static char *dup_bytes(const uint8_t *p, size_t n) {
    char *out = malloc(n + 1);
    if (!out)
        return NULL;
    if (n)
        memcpy(out, p, n);
    out[n] = '\0'; /* convenient for text output; length is reported too */
    return out;
}

int wk_run(const char *path, const char *const *argv,
           const char *stdin_data, size_t stdin_len, wk_result *out) {
    memset(out, 0, sizeof *out);

    exec_host_string_t wpath;
    exec_host_string_set(&wpath, path);

    size_t argc = 0;
    if (argv)
        while (argv[argc])
            argc++;
    exec_host_list_string_t wargs = {NULL, 0};
    if (argc) {
        wargs.ptr = malloc(argc * sizeof *wargs.ptr);
        if (!wargs.ptr) {
            out->error = strdup("out of memory");
            return -1;
        }
        wargs.len = argc;
        for (size_t i = 0; i < argc; i++)
            exec_host_string_set(&wargs.ptr[i], argv[i]);
    }

    /* The child inherits no environment of its own here; wk gives it the
     * node's. (Adding per-call env is a matter of filling this list.) */
    exec_host_list_tuple2_string_string_t wenv = {NULL, 0};

    exec_host_list_u8_t win = {NULL, 0};
    if (stdin_len) {
        win.ptr = (uint8_t *)stdin_data;
        win.len = stdin_len;
    }

    wk_exec_process_output_t res;
    exec_host_string_t err;
    bool ok = wk_exec_process_run(&wpath, &wargs, &wenv, &win, &res, &err);
    free(wargs.ptr);

    if (!ok) {
        out->error = dup_bytes(err.ptr, err.len);
        exec_host_string_free(&err);
        return -1;
    }
    out->exit_code = res.exit_code;
    out->stdout_data = dup_bytes(res.stdout.ptr, res.stdout.len);
    out->stdout_len = res.stdout.len;
    out->stderr_data = dup_bytes(res.stderr.ptr, res.stderr.len);
    out->stderr_len = res.stderr.len;
    /* wit-bindgen emits no free for the record itself, only for its lists;
     * the bytes were copied out above, so release the originals. */
    exec_host_list_u8_free(&res.stdout);
    exec_host_list_u8_free(&res.stderr);
    return 0;
}

void wk_result_free(wk_result *r) {
    free(r->stdout_data);
    free(r->stderr_data);
    free(r->error);
    memset(r, 0, sizeof *r);
}

/* --- pipelines: spawn/wait --------------------------------------------- */

wk_pipe wk_pipe_new(void) {
    wk_pipe p;
    p.h = wk_exec_process_constructor_pipe().__handle;
    return p;
}

void wk_pipe_free(wk_pipe p) {
    wk_exec_process_own_pipe_t own = {p.h};
    wk_exec_process_pipe_drop_own(own);
}

/* Fill in one of the WIT stdio variants from a wk_stdio. */
static void set_stdin(wk_exec_process_stdin_from_t *v, const wk_stdio *s) {
    if (s && s->pipe) {
        v->tag = WK_EXEC_PROCESS_STDIN_FROM_PIPE_END;
        v->val.pipe_end.__handle = s->pipe->h;
    } else if (s && s->len) {
        v->tag = WK_EXEC_PROCESS_STDIN_FROM_BYTES;
        v->val.bytes.ptr = (uint8_t *)s->bytes;
        v->val.bytes.len = s->len;
    } else {
        v->tag = WK_EXEC_PROCESS_STDIN_FROM_EMPTY;
    }
}

static void set_stdout(wk_exec_process_stdout_to_t *v, const wk_stdio *s) {
    if (s && s->pipe) {
        v->tag = WK_EXEC_PROCESS_STDOUT_TO_PIPE_END;
        v->val.pipe_end.__handle = s->pipe->h;
    } else {
        v->tag = WK_EXEC_PROCESS_STDOUT_TO_CAPTURE;
    }
}

int wk_spawn(const char *path, const char *const *argv,
             const wk_stdio *in, const wk_stdio *out_io, const wk_stdio *err_io,
             wk_child *out, char **error) {
    if (error)
        *error = NULL;

    exec_host_string_t wpath;
    exec_host_string_set(&wpath, path);

    size_t argc = 0;
    if (argv)
        while (argv[argc])
            argc++;
    exec_host_list_string_t wargs = {NULL, 0};
    if (argc) {
        wargs.ptr = malloc(argc * sizeof *wargs.ptr);
        if (!wargs.ptr) {
            if (error)
                *error = strdup("out of memory");
            return -1;
        }
        wargs.len = argc;
        for (size_t i = 0; i < argc; i++)
            exec_host_string_set(&wargs.ptr[i], argv[i]);
    }
    exec_host_list_tuple2_string_string_t wenv = {NULL, 0};

    wk_exec_process_stdin_from_t win;
    wk_exec_process_stdout_to_t wout, werr;
    set_stdin(&win, in);
    set_stdout(&wout, out_io);
    set_stdout(&werr, err_io);

    wk_exec_process_own_child_t child;
    exec_host_string_t err;
    bool ok = wk_exec_process_spawn(&wpath, &wargs, &wenv, &win, &wout, &werr,
                                    &child, &err);
    free(wargs.ptr);
    if (!ok) {
        if (error)
            *error = dup_bytes(err.ptr, err.len);
        exec_host_string_free(&err);
        return -1;
    }
    out->h = child.__handle;
    return 0;
}

int wk_wait(wk_child child, wk_result *out) {
    memset(out, 0, sizeof *out);
    wk_exec_process_borrow_child_t b = {child.h};
    wk_exec_process_output_t res;
    exec_host_string_t err;
    bool ok = wk_exec_process_method_child_wait(b, &res, &err);
    /* `wait` reaps: the handle is spent either way. */
    wk_exec_process_own_child_t own = {child.h};
    wk_exec_process_child_drop_own(own);

    if (!ok) {
        out->error = dup_bytes(err.ptr, err.len);
        exec_host_string_free(&err);
        return -1;
    }
    out->exit_code = res.exit_code;
    out->stdout_data = dup_bytes(res.stdout.ptr, res.stdout.len);
    out->stdout_len = res.stdout.len;
    out->stderr_data = dup_bytes(res.stderr.ptr, res.stderr.len);
    out->stderr_len = res.stderr.len;
    /* wit-bindgen emits no free for the record itself, only for its lists;
     * the bytes were copied out above, so release the originals. */
    exec_host_list_u8_free(&res.stdout);
    exec_host_list_u8_free(&res.stderr);
    return 0;
}
