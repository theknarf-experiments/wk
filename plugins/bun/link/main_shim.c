#include <stdint.h>
extern void init_argv(int, char**)  __asm__("_RNvNtCshYsCSgnrVY4_8bun_core4util9init_argv");
extern void crash_init(void)      __asm__("_RNvNtCsa9HHPoMpNfP_17bun_crash_handler5draft4init");
extern void stdio_init(void)      __asm__("_RNvNtNtCshYsCSgnrVY4_8bun_core6output5stdio4init");
extern void stackcheck_init(void) __asm__("Bun__StackCheck__initialize");
extern void cli_start(void)       __asm__("_RNvNtNtCs76GMnEbqh9K_11bun_runtime3cli3cli5start");
extern int  bun_signal_ignore(int) __asm__("signal_placeholder"); // not used
extern long write(int, const void*, unsigned long);
extern void* signal(int, void*);
int __main_argc_argv(int argc, char** argv) {
    write(2,"a",1); init_argv(argc, argv);
    write(2,"b",1); crash_init();
    write(2,"c",1); signal(13, (void*)1); /* SIGPIPE, SIG_IGN */
    write(2,"d",1); stdio_init();
    write(2,"e",1); stackcheck_init();
    write(2,"f",1); cli_start();
    write(2,"g",1);
    return 0;
}
