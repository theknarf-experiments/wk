// Minimal pwd stub for wasi (no passwd database).
#ifndef _WASI_COMPAT_PWD_H
#define _WASI_COMPAT_PWD_H
#include <sys/types.h>
#include <stddef.h>
struct passwd { char *pw_name; char *pw_passwd; uid_t pw_uid; gid_t pw_gid; char *pw_gecos; char *pw_dir; char *pw_shell; };
#ifdef __cplusplus
extern "C" {
#endif
static inline struct passwd *getpwuid(uid_t u){(void)u;return 0;}
static inline int getpwuid_r(uid_t u,struct passwd*p,char*b,size_t s,struct passwd**r){(void)u;(void)p;(void)b;(void)s;if(r)*r=0;return 0;}
#ifdef __cplusplus
}
#endif
#endif
