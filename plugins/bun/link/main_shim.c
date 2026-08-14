// wasi entry for bun. wasi-libc's __main_void calls __main_argc_argv; instead of
// bun's Rust `main` (whose exact compiled form deadlocks in early startup on
// wasm — under investigation), replicate main's startup sequence explicitly by
// calling the same crate entry points in order. The steps bun's main additionally
// runs (output::flush_guard, ParentDeathWatchdog::install) are no-ops on wasi.
// Kept alive as a GC root via -Wl,--export=__main_argc_argv (the __main_void
// reference is weak; gc-sections would otherwise drop it and wasm-ld would leave
// __main_void calling an infinite-loop undefined-weak stub).
extern void init_argv(int, char**)  __asm__("_RNvNtCshYsCSgnrVY4_8bun_core4util9init_argv");
extern void crash_init(void)        __asm__("_RNvNtCsa9HHPoMpNfP_17bun_crash_handler5draft4init");
extern void stdio_init(void)        __asm__("_RNvNtNtCshYsCSgnrVY4_8bun_core6output5stdio4init");
extern void stackcheck_init(void)   __asm__("Bun__StackCheck__initialize");
extern void cli_start(void)         __asm__("_RNvNtNtCs76GMnEbqh9K_11bun_runtime3cli3cli5start");
extern void* signal(int, void*);
int __main_argc_argv(int argc, char** argv) {
    init_argv(argc, argv);
    crash_init();
    signal(13, (void*)1); /* SIGPIPE  -> SIG_IGN */
    signal(25, (void*)1); /* SIGXFSZ  -> SIG_IGN */
    stdio_init();
    stackcheck_init();
    cli_start();          /* diverges (Global::exit) for most commands */
    return 0;
}
