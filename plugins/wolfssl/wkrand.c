/* wolfSSL seed source on WASI — see wkrand.h. getentropy() caps a single
 * request at 256 bytes (POSIX GETENTROPY_MAX), so chunk; wolfSSL's DRBG asks
 * for ~100 bytes per reseed normally but some configs ask for more.
 * Returns 0 on success, nonzero on failure (wc_GenerateSeed's contract). */
#include <unistd.h>

#include "wkrand.h"

int wk_getentropy_seed(unsigned char *output, unsigned int sz) {
    while (sz > 0) {
        unsigned int chunk = sz > 256 ? 256 : sz;
        if (getentropy(output, chunk) != 0) {
            return -1;
        }
        output += chunk;
        sz -= chunk;
    }
    return 0;
}
