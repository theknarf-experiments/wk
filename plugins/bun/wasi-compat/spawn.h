// Minimal posix_spawn stub for wasi (no subprocess; spawn paths are inert).
#ifndef _WASI_COMPAT_SPAWN_H
#define _WASI_COMPAT_SPAWN_H
#include <sys/types.h>
#include <errno.h>
typedef struct { int __unused; } posix_spawnattr_t;
typedef struct { int __unused; } posix_spawn_file_actions_t;
#ifdef __cplusplus
extern "C" {
#endif
static inline int posix_spawn(pid_t *p, const char *path, const posix_spawn_file_actions_t *fa, const posix_spawnattr_t *at, char *const av[], char *const ev[]) { (void)p;(void)path;(void)fa;(void)at;(void)av;(void)ev; return ENOSYS; }
static inline int posix_spawnp(pid_t *p, const char *file, const posix_spawn_file_actions_t *fa, const posix_spawnattr_t *at, char *const av[], char *const ev[]) { (void)p;(void)file;(void)fa;(void)at;(void)av;(void)ev; return ENOSYS; }
static inline int posix_spawnattr_init(posix_spawnattr_t *a){(void)a;return 0;}
static inline int posix_spawnattr_destroy(posix_spawnattr_t *a){(void)a;return 0;}
static inline int posix_spawn_file_actions_init(posix_spawn_file_actions_t *a){(void)a;return 0;}
static inline int posix_spawn_file_actions_destroy(posix_spawn_file_actions_t *a){(void)a;return 0;}
#ifdef __cplusplus
}
#endif
#endif
