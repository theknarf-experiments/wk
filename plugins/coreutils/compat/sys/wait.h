/* <sys/wait.h> for wasm32-wasi: there are no child processes to wait for.
 * A few coreutils programs (install --strip, and the excluded env/nohup
 * family) include this header; the macros keep them compiling, and compat.c
 * implements the calls as ENOSYS so the failure is honest at runtime rather
 * than at build time. */
#ifndef _WK_COMPAT_SYS_WAIT_H
#define _WK_COMPAT_SYS_WAIT_H

#include <sys/types.h>

#define WNOHANG 1
#define WUNTRACED 2

#define WEXITSTATUS(s) (((s) & 0xff00) >> 8)
#define WTERMSIG(s) ((s) & 0x7f)
#define WSTOPSIG(s) WEXITSTATUS(s)
#define WIFEXITED(s) (WTERMSIG(s) == 0)
#define WIFSTOPPED(s) ((short)((((s) & 0xffff) * 0x10001) >> 8) > 0x7f00)
#define WIFSIGNALED(s) (((s) & 0xffff) - 1U < 0xffu)
#define WCOREDUMP(s) ((s) & 0x80)
#define WIFCONTINUED(s) ((s) == 0xffff)

pid_t wait(int *status);
pid_t waitpid(pid_t pid, int *status, int options);

#endif
