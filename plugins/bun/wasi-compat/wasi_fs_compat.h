#pragma once
#if defined(__wasi__)
#include <errno.h>
#ifdef __cplusplus
extern "C" {
#endif
static inline int fchdir(int fd){(void)fd;errno=ENOSYS;return -1;}
#ifdef __cplusplus
}
#endif
#endif
