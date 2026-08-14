// Single-allocator unification. The component canonical ABI's cabi_realloc
// (wit-bindgen, in libbun_rust) uses mimalloc, and bun's Rust global allocator
// is mimalloc. wasi-libc's malloc/free/realloc default to dlmalloc, so memory
// the component model hands to wasi-libc (e.g. readdir entry names allocated by
// cabi_realloc) was freed by dlmalloc -> heap corruption. Route the C allocator
// to mimalloc so every allocation shares one heap. Linked with
// --allow-multiple-definition (this object first) to preempt libc's dlmalloc.
#include <stddef.h>
extern void* mi_malloc(size_t);
extern void  mi_free(void*);
extern void* mi_realloc(void*, size_t);
extern void* mi_calloc(size_t, size_t);
extern void* mi_aligned_alloc(size_t, size_t);
extern size_t mi_usable_size(void*);
void* malloc(size_t n) { return mi_malloc(n); }
void  free(void* p) { mi_free(p); }
void* realloc(void* p, size_t n) { return mi_realloc(p, n); }
void* calloc(size_t a, size_t b) { return mi_calloc(a, b); }
void* aligned_alloc(size_t al, size_t n) { return mi_aligned_alloc(al, n); }
size_t malloc_usable_size(void* p) { return mi_usable_size(p); }
