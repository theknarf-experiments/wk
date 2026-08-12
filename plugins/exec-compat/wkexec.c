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
    wk_exec_process_output_free(&res);
    return 0;
}

void wk_result_free(wk_result *r) {
    free(r->stdout_data);
    free(r->stderr_data);
    free(r->error);
    memset(r, 0, sizeof *r);
}
