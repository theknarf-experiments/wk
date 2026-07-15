/* <sys/wait.h> for wasm32-wasi: WASI has no processes, so there is nothing to
 * wait on. Vim compiles its job/shell-out code unconditionally but it is never
 * reached at runtime (fork/exec always fail). These macros/prototypes just let
 * that code build; waitpid() is stubbed in compat/wkos.c to report "no child". */
#ifndef _WK_COMPAT_SYS_WAIT_H
#define _WK_COMPAT_SYS_WAIT_H

#include <sys/types.h>

#define WNOHANG   1
#define WUNTRACED 2

#define WIFEXITED(s)    (((s) & 0x7f) == 0)
#define WEXITSTATUS(s)  (((s) >> 8) & 0xff)
#define WIFSIGNALED(s)  (((s) & 0x7f) != 0 && ((s) & 0x7f) != 0x7f)
#define WTERMSIG(s)     ((s) & 0x7f)
#define WIFSTOPPED(s)   (((s) & 0xff) == 0x7f)
#define WSTOPSIG(s)     WEXITSTATUS(s)

pid_t waitpid(pid_t pid, int *status, int options);
pid_t wait(int *status);
pid_t wait4(pid_t pid, int *status, int options, void *rusage);

#endif
