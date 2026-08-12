/* `seq 1 200000 | head -1` — the thing wk_run() cannot do.
 *
 * With run(), the producer must finish before the consumer starts, so this
 * would generate 200000 lines (about 1.3 MB) and then throw all but one away.
 * With spawn() both children are live on a 64 KiB pipe: seq fills it and
 * waits, head takes its line and exits, and seq's next write fails because
 * nobody is reading — a pipeline that stops early, like a real shell's. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "wkexec.h"

#define COREUTILS "/bin/coreutils.wasm"

int main(void) {
    wk_pipe p = wk_pipe_new();

    const char *seq_argv[] = {"seq", "1", "200000", NULL};
    const char *head_argv[] = {"head", "-1", NULL};

    wk_stdio to_pipe = {&p, NULL, 0};
    wk_stdio from_pipe = {&p, NULL, 0};

    wk_child producer, consumer;
    char *err = NULL;

    if (wk_spawn(COREUTILS, seq_argv, NULL, &to_pipe, NULL, &producer, &err)) {
        printf("spawn seq: %s\n", err);
        return 1;
    }
    if (wk_spawn(COREUTILS, head_argv, &from_pipe, NULL, NULL, &consumer, &err)) {
        printf("spawn head: %s\n", err);
        return 1;
    }

    /* head's output is captured, so it comes back here. */
    wk_result hr;
    if (wk_wait(consumer, &hr)) {
        printf("wait head: %s\n", hr.error);
        return 1;
    }
    printf("head got: %.*s", (int)hr.stdout_len, hr.stdout_data);
    printf("head exit: %d\n", hr.exit_code);
    wk_result_free(&hr);

    /* seq is expected to die writing into a pipe nobody reads any more —
     * exactly what a shell reports when `head` leaves early. */
    wk_result sr;
    if (wk_wait(producer, &sr))
        printf("seq ended: %s\n", sr.error);
    else
        printf("seq exit: %d\n", sr.exit_code);
    wk_result_free(&sr);

    wk_pipe_free(p);
    printf("pipeline done\n");
    return 0;
}
