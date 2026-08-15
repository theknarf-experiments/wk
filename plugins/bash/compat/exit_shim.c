#include <stdint.h>
// Route libc process exit through wasi:cli/exit.exit-with-code so the child's
// real status reaches the wk host. wasi-sdk's default exit() lowers to
// wasi:cli/exit.exit(result) — a boolean (ok/err) — so every non-zero code
// collapses to 1 before it leaves the guest. exit-with-code carries the full
// u8; the host (wasmtime-wasi) raises it as I32Exit(code), which wk:exec
// returns verbatim. Overriding _Exit/_exit (the lowest libc exit primitives)
// keeps exit()'s atexit handling intact while replacing only the final lowering.
__attribute__((import_module("wasi:cli/exit@0.2.12"), import_name("exit-with-code")))
extern void __wk_exit_with_code(int32_t code);

_Noreturn void _Exit(int code) { __wk_exit_with_code(code & 0xff); __builtin_unreachable(); }
_Noreturn void _exit(int code) { __wk_exit_with_code(code & 0xff); __builtin_unreachable(); }
