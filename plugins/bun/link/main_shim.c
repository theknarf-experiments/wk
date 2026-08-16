// wasi entry for bun. wasi-libc's __main_void calls __main_argc_argv; instead of
// bun's Rust `main` (whose compiled form deadlocks in early startup on wasm),
// replicate main's startup by calling the same crate entry points in order.
// Kept as a GC root via -Wl,--export=__main_argc_argv.
extern void init_argv(int, char**)  __asm__("_RNvNtCshYsCSgnrVY4_8bun_core4util9init_argv");
extern void crash_init(void)        __asm__("_RNvNtCsa9HHPoMpNfP_17bun_crash_handler5draft4init");
extern void stdio_init(void)        __asm__("_RNvNtNtCshYsCSgnrVY4_8bun_core6output5stdio4init");
extern void stackcheck_init(void)   __asm__("Bun__StackCheck__initialize");
extern void cli_start(void)         __asm__("_RNvNtNtCs76GMnEbqh9K_11bun_runtime3cli3cli5start");
extern void* signal(int, void*);
// Populate `environ` here (post-ctors) rather than in wasi-libc's eager ctor:
// mimalloc's first-init clock read traps during __wasm_call_ctors, so the eager
// init is deferred (see environ_defer.c) and driven once from here, where the
// wasip2 monotonic clock is callable and before bun reads process.env.
extern void __wasilibc_initialize_environ(void);
// mimalloc's arena/hole purging reads the monotonic clock to time when to
// decommit freed pages. On wasm that clock is a component import; when a purge
// fires from inside `cabi_realloc` (e.g. the host lowering an input-stream.read
// result during a TCP recv), calling the import traps "cannot leave component
// instance". Purging can't hand memory back to an OS on wasm anyway (linear
// memory only grows), so disable it — freed pages stay in mimalloc's own free
// lists for reuse, and the clock is never called from the allocator.
#include <mimalloc.h>
// Honor a working directory handed down by the launcher. wk:exec can't set a
// child's initial cwd (it's wasi-libc state, "/" until something chdir()s), so
// Bun.spawn's `cwd` (and a shell `cd` before an external command) arrives as
// __WK_EXEC_CWD in the env. bash/coreutils do this in a C constructor, but bun
// defers environ init (above), so a constructor's getenv would see nothing —
// chdir here, right after environ is populated, so a nested bun honors `cwd`.
#include <stdlib.h>
#include <unistd.h>
int __main_argc_argv(int argc, char** argv) {
    mi_option_set(mi_option_purge_delay, -1);
    mi_option_set_enabled(mi_option_purge_holes, false);
    __wasilibc_initialize_environ();
    {
        const char* wk_cwd = getenv("__WK_EXEC_CWD");
        if (wk_cwd && *wk_cwd) (void)chdir(wk_cwd);
    }
    init_argv(argc, argv);
    crash_init();
    signal(13, (void*)1); /* SIGPIPE  -> SIG_IGN */
    signal(25, (void*)1); /* SIGXFSZ  -> SIG_IGN */
    stdio_init();
    stackcheck_init();
    cli_start();
    return 0;
}
