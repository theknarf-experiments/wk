/* Entropy seed for wolfSSL on WASI: wasi-libc has getentropy() (host
 * wasi:random behind it) but no getrandom() symbol, and wolfSSL's generic
 * unix seed path wants /dev/urandom, which WASI doesn't have. random.c is
 * compiled with -DCUSTOM_RAND_GENERATE_SEED=wk_getentropy_seed and this
 * header force-included (clang C99 would otherwise error on the implicit
 * declaration of the macro's expansion). */
#ifndef WK_WOLFSSL_RAND_H
#define WK_WOLFSSL_RAND_H

int wk_getentropy_seed(unsigned char *output, unsigned int sz);

#endif
