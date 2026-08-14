// wasi-libc eagerly populates `environ` from a ctor in __wasm_call_ctors. On
// wasip2 that allocates (cabi_realloc -> mimalloc), and mimalloc's first-init
// reads the monotonic clock — which traps when called during ctors (the clock
// import isn't callable yet; it works fine post-instantiation). Skip the eager
// ctor call so environ initializes lazily on the first getenv/environ access
// after startup, where the clock works. --wrap redirects every reference.
extern void __real___wasilibc_initialize_environ(void);
static unsigned calls = 0;
void __wrap___wasilibc_initialize_environ(void) {
    // Call #0 is the eager ctor; defer it. Lazy accesses (ensure_environ) then
    // find environ uninitialized and re-enter here to do the real init.
    if (calls++ == 0) return;
    __real___wasilibc_initialize_environ();
}
