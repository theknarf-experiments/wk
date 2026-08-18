/* Stub <sys/wait.h> for the pdftex cross build: WASI has no processes, so
 * nothing here can ever run — kpathsea's tex-make.c (mktexpk fallback) merely
 * needs the declarations to compile. The functions come from wasi-shim.c and
 * fail cleanly with ENOSYS.
 */
#ifndef WK_DOCTOOLS_SYS_WAIT_H
#define WK_DOCTOOLS_SYS_WAIT_H

#include <sys/types.h> /* pid_t */

#define WNOHANG 1
#define WUNTRACED 2

#define WIFEXITED(s) (((s) & 0x7f) == 0)
#define WEXITSTATUS(s) (((s) >> 8) & 0xff)
#define WIFSIGNALED(s) (!WIFEXITED(s))
#define WTERMSIG(s) ((s) & 0x7f)
#define WIFSTOPPED(s) (0)
#define WSTOPSIG(s) (0)

extern int fork(void);
extern int waitpid(int pid, int *status, int options);
extern int wait(int *status);

#endif
