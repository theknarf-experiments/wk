/* Turning a pipe file descriptor back into the wk pipe behind it.
 *
 * Needed when a program is not going to read or write the pipe itself but to
 * hand it to something else — a shell wiring `a | b`, where each stage's stdio
 * must *be* the pipe. See pipe.c. */
#ifndef WK_PIPE_H
#define WK_PIPE_H

#include <stdbool.h>

#include "exec_host.h"

bool wk_pipe_of_fd(int fd, wk_exec_process_borrow_pipe_t *out);

#endif
