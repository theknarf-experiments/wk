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

int wk_run_env(const char *path, const char *const *argv,
               const char *const *envp, const char *cwd,
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

    /* Per-call environment, split from the caller's `KEY=VALUE` vector. When
     * `envp` is null the child inherits the node's environment (wk supplies
     * it); when given, this is the child's environment (e.g. Bun.spawn's
     * `env`). */
    size_t envc = 0;
    if (envp)
        while (envp[envc])
            envc++;
    /* wk:exec cannot set the child's cwd, so pass it as __WK_EXEC_CWD; a
     * chdir constructor (chdir_shim.c, linked into the shell and coreutils)
     * reads it and chdir()s before main. Only meaningful with a non-empty env
     * (an empty env means "inherit the node's", which carries no cwd). */
    int add_cwd = cwd && *cwd;
    exec_host_list_tuple2_string_string_t wenv = {NULL, 0};
    if (envc || add_cwd) {
        wenv.ptr = malloc((envc + (add_cwd ? 1 : 0)) * sizeof *wenv.ptr);
        if (!wenv.ptr) {
            free(wargs.ptr);
            out->error = strdup("out of memory");
            return -1;
        }
        for (size_t i = 0; i < envc; i++) {
            const char *eq = strchr(envp[i], '=');
            size_t klen = eq ? (size_t)(eq - envp[i]) : strlen(envp[i]);
            wenv.ptr[i].f0.ptr = (uint8_t *)envp[i];
            wenv.ptr[i].f0.len = klen;
            wenv.ptr[i].f1.ptr = (uint8_t *)(eq ? eq + 1 : "");
            wenv.ptr[i].f1.len = eq ? strlen(eq + 1) : 0;
        }
        if (add_cwd) {
            static const char k[] = "__WK_EXEC_CWD";
            wenv.ptr[envc].f0.ptr = (uint8_t *)k;
            wenv.ptr[envc].f0.len = sizeof k - 1;
            wenv.ptr[envc].f1.ptr = (uint8_t *)cwd;
            wenv.ptr[envc].f1.len = strlen(cwd);
        }
        wenv.len = envc + (add_cwd ? 1 : 0);
    }

    exec_host_list_u8_t win = {NULL, 0};
    if (stdin_len) {
        win.ptr = (uint8_t *)stdin_data;
        win.len = stdin_len;
    }

    wk_exec_process_output_t res;
    exec_host_string_t err;
    bool ok = wk_exec_process_run(&wpath, &wargs, &wenv, &win, &res, &err);
    free(wargs.ptr);
    free(wenv.ptr);

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

/* Env-less form: the child inherits the node's environment, cwd "/". */
int wk_run(const char *path, const char *const *argv,
           const char *stdin_data, size_t stdin_len, wk_result *out) {
    return wk_run_env(path, argv, NULL, NULL, stdin_data, stdin_len, out);
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
    if (s && s->pipe_borrow) {
        v->tag = WK_EXEC_PROCESS_STDIN_FROM_PIPE_END;
        v->val.pipe_end = *s->pipe_borrow;
    } else if (s && s->pipe) {
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
    if (s && s->pipe_borrow) {
        v->tag = WK_EXEC_PROCESS_STDOUT_TO_PIPE_END;
        v->val.pipe_end = *s->pipe_borrow;
    } else if (s && s->pipe) {
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

/* Fill `wenv` from a `KEY=VALUE` vector plus an optional __WK_EXEC_CWD entry.
 * Caller frees `wenv->ptr`. Returns 0, or -1 on OOM. */
static int build_wenv(const char *const *envp, const char *cwd,
                      exec_host_list_tuple2_string_string_t *wenv) {
    wenv->ptr = NULL;
    wenv->len = 0;
    size_t envc = 0;
    if (envp)
        while (envp[envc])
            envc++;
    int add_cwd = cwd && *cwd;
    if (!envc && !add_cwd)
        return 0;
    wenv->ptr = malloc((envc + (add_cwd ? 1 : 0)) * sizeof *wenv->ptr);
    if (!wenv->ptr)
        return -1;
    for (size_t i = 0; i < envc; i++) {
        const char *eq = strchr(envp[i], '=');
        size_t klen = eq ? (size_t)(eq - envp[i]) : strlen(envp[i]);
        wenv->ptr[i].f0.ptr = (uint8_t *)envp[i];
        wenv->ptr[i].f0.len = klen;
        wenv->ptr[i].f1.ptr = (uint8_t *)(eq ? eq + 1 : "");
        wenv->ptr[i].f1.len = eq ? strlen(eq + 1) : 0;
    }
    if (add_cwd) {
        static const char k[] = "__WK_EXEC_CWD";
        wenv->ptr[envc].f0.ptr = (uint8_t *)k;
        wenv->ptr[envc].f0.len = sizeof k - 1;
        wenv->ptr[envc].f1.ptr = (uint8_t *)cwd;
        wenv->ptr[envc].f1.len = strlen(cwd);
    }
    wenv->len = envc + (add_cwd ? 1 : 0);
    return 0;
}

/* Like wk_spawn, but with a per-call environment and working directory (the
 * same __WK_EXEC_CWD mechanism as wk_run_env). */
int wk_spawn_env(const char *path, const char *const *argv,
                 const char *const *envp, const char *cwd, const wk_stdio *in,
                 const wk_stdio *out_io, const wk_stdio *err_io, wk_child *out,
                 char **error) {
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

    exec_host_list_tuple2_string_string_t wenv;
    if (build_wenv(envp, cwd, &wenv) != 0) {
        free(wargs.ptr);
        if (error)
            *error = strdup("out of memory");
        return -1;
    }

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
    free(wenv.ptr);
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

void wk_child_detach(wk_child child) {
    wk_exec_process_own_child_t own = {child.h};
    wk_exec_process_child_drop_own(own);
}
