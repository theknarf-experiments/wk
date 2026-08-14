// Minimal grp stub for wasi (no group database).
#ifndef _WASI_COMPAT_GRP_H
#define _WASI_COMPAT_GRP_H
#include <sys/types.h>
#include <stddef.h>
struct group { char *gr_name; char *gr_passwd; gid_t gr_gid; char **gr_mem; };
#ifdef __cplusplus
extern "C" {
#endif
static inline struct group *getgrgid(gid_t g){(void)g;return 0;}
static inline struct group *getgrnam(const char *n){(void)n;return 0;}
static inline int getgrgid_r(gid_t g,struct group*grp,char*b,size_t s,struct group**r){(void)g;(void)grp;(void)b;(void)s;if(r)*r=0;return 0;}
static inline int getgrnam_r(const char*n,struct group*grp,char*b,size_t s,struct group**r){(void)n;(void)grp;(void)b;(void)s;if(r)*r=0;return 0;}
static inline int getgrouplist(const char*u,gid_t g,gid_t*grps,int*n){(void)u;(void)g;(void)grps;if(n)*n=0;return 0;}
#ifdef __cplusplus
}
#endif
#endif
