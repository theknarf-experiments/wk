/* A demonstration that a program inside a wk node can run *other real
 * programs* — the thing WASI's missing fork/exec otherwise makes impossible.
 *
 * It runs the cross-compiled GNU coreutils multicall binary from its own
 * filesystem (the Dockerfile puts it at /bin/coreutils.wasm), several tools in
 * sequence, and then a two-stage pipeline by feeding the first program's
 * stdout into the second's stdin — which is how a shell would do it with this
 * capability. Note argv[0]: "ls", "seq", "wc" — coreutils dispatches on it,
 * so one binary is a hundred commands, exactly as its symlink install does.
 */
#include <stdio.h>
#include <string.h>

#include "wkexec.h"

#define COREUTILS "/bin/coreutils.wasm"

static void show(const char *label, wk_result *r) {
    if (r->error) {
        printf("%-22s ERROR: %s\n", label, r->error);
        return;
    }
    printf("%-22s exit=%d out=%.*s", label, r->exit_code,
           (int)r->stdout_len, r->stdout_data);
    if (r->stdout_len == 0 || r->stdout_data[r->stdout_len - 1] != '\n')
        printf("\n");
    if (r->stderr_len)
        printf("%-22s err=%.*s\n", "", (int)r->stderr_len, r->stderr_data);
}

int main(void) {
    wk_result r;

    printf("wk:exec demo — running real programs from inside a node\n\n");

    /* 1. A plain command. */
    const char *echo_args[] = {"echo", "hello", "from", "an", "exec'd", "program", NULL};
    if (wk_run(COREUTILS, echo_args, NULL, 0, &r) == 0 || r.error)
        show("echo:", &r);
    wk_result_free(&r);

    /* 2. Something that reads the shared filesystem: the child sees the same
     *    files as its parent. */
    const char *ls_args[] = {"ls", "-1", "/", NULL};
    wk_run(COREUTILS, ls_args, NULL, 0, &r);
    show("ls -1 /:", &r);
    wk_result_free(&r);

    /* 3. A pipeline: seq's output becomes wc's input. No fork needed — the
     *    caller carries the bytes between the two runs. */
    const char *seq_args[] = {"seq", "1", "5", NULL};
    wk_run(COREUTILS, seq_args, NULL, 0, &r);
    show("seq 1 5:", &r);
    if (!r.error) {
        char *piped = r.stdout_data;
        size_t piped_len = r.stdout_len;
        wk_result r2;
        const char *wc_args[] = {"wc", "-l", NULL};
        wk_run(COREUTILS, wc_args, piped, piped_len, &r2);
        show("seq 1 5 | wc -l:", &r2);
        wk_result_free(&r2);
    }
    wk_result_free(&r);

    /* 4. Writes by the child are visible to the parent afterwards. */
    const char *mkdir_args[] = {"mkdir", "-p", "/made-by-child", NULL};
    wk_run(COREUTILS, mkdir_args, NULL, 0, &r);
    show("mkdir /made-by-child:", &r);
    wk_result_free(&r);
    wk_run(COREUTILS, ls_args, NULL, 0, &r);
    show("ls -1 / (again):", &r);
    wk_result_free(&r);

    /* 5. Failure paths report cleanly rather than trapping. */
    const char *none[] = {"nope", NULL};
    wk_run("/nope.wasm", none, NULL, 0, &r);
    show("missing program:", &r);
    wk_result_free(&r);

    return 0;
}
