/* wk-wasi-threads.c — the timed-wait override for LibreOffice on wasm32-wasip2.
 *
 * WHY THIS FILE EXISTS
 * ====================
 * wasm32-wasip2 has no threads, but wasi-libc still *declares* the whole
 * pthread API and libc++'s __config_site still sets _LIBCPP_HAS_THREADS=1.
 * Everything therefore compiles and links, and the trouble is all at run time.
 * Two of those runtime behaviours are wrong for a single-threaded process in
 * ways that kill LibreOffice within milliseconds of start-up:
 *
 *   1. wasi-libc's pthread_cond_timedwait() (libc.a, pthread_cond_timedwait.c.obj)
 *      is musl's, and it implements the wait as
 *
 *          clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME, abstime, 0)
 *
 *      On wasip2 that call returns ENOTSUP (58) *immediately* — verified by
 *      running: a CLOCK_MONOTONIC sleep, relative or absolute, works and takes
 *      the requested time; both CLOCK_REALTIME forms return 58 in 0.0 ms.
 *      So pthread_cond_timedwait returns 58 rather than ETIMEDOUT (73), and
 *      libc++'s condition_variable::__do_timed_wait is
 *
 *          if (ec != 0 && ec != ETIMEDOUT) __throw_system_error(...)
 *
 *      inside a noexcept function — so it goes __throw_system_error ->
 *      std::terminate -> abort -> `unreachable`, exit 134. Reproduced under
 *      wasmtime with a five-line program before this file was written.
 *
 *      That is not an exotic path. sal/osl/unx/conditn.cxx:119
 *      (osl_waitCondition, the bottom of every osl::Condition in LibreOffice)
 *      is `m_Condition.wait_for(g, duration, predicate)`, and
 *      vcl/headless/svpinst.cxx:503 (SvpSalInstance::ImplYield, the main event
 *      loop) is the same shape. Without this override the process aborts as
 *      soon as anything waits with a timeout.
 *
 *   2. wasi-libc's pthread_cond_wait() is a bare `unreachable` instruction.
 *      That one is *correct* and we deliberately keep the crash; see below.
 *
 * THE POLICY (PORTING.md, "Policy decisions", item 2)
 * ==================================================
 * A timed wait that can only ever time out must RETURN TIMEOUT, not abort.
 * In a process that cannot spawn a thread — pthread_create returns ENOTSUP —
 * nothing can ever signal a condition variable, so "the deadline passed" is
 * the truthful answer, and every caller already has a code path for it.
 *
 * WHY THE SLEEP IS KEPT
 * =====================
 * Returning ETIMEDOUT *immediately* would also be "truthful", and it would be
 * a disaster: SvpSalInstance::ImplYield uses the timed wait as the event
 * loop's idle sleep. An instant return turns the LibreOffice main loop into a
 * busy spin at 100% CPU. So this override does the sleep the caller asked for,
 * on the clock that works, and only then reports the timeout.
 *
 * WHAT THIS IS NOT
 * ================
 * It is not a patch to LibreOffice and it is not a patch to wasi-libc. It is a
 * separate static archive linked ahead of libc on every LibreOffice link (see
 * gb_LinkTarget_LDFLAGS in solenv/gbuild/platform/WASI_INTEL_GCC.mk, added by
 * patches/core-0002-gbuild-wasi-platform.patch). Overriding here rather than in
 * LibreOffice's tree means it also covers the ~149 external libraries, libc++
 * and libc++abi, none of which we patch.
 *
 * ARCHIVE ORDER, WHICH IS NOT A DETAIL
 * ====================================
 * lld registers archive members as lazy symbols and fetches one when a later
 * reference appears, so the FIRST archive to offer a name wins — being the
 * strong definition does not help if libc offered the weak one first. Verified
 * both ways with the five-line repro: with libc.a named explicitly ahead of
 * this archive, the abort comes straight back, and `-Wl,--why-extract` shows
 * libc's member being the one pulled.
 *
 * The makefile therefore links this archive with
 * -Wl,--whole-archive ... -Wl,--no-whole-archive. That force-includes the
 * object instead of leaving it lazy, so the definition is present before any
 * reference is resolved and the override cannot lose a race with libc no
 * matter where it lands on the link line. Verified: with --whole-archive, even
 * libc.a named first links to this definition. The archive has exactly one
 * member, so "whole" costs nothing.
 *
 * Nothing collides: in libc.a, pthread_cond_timedwait is a WEAK alias of
 * __pthread_cond_timedwait and pthread_cond_wait sits alone in its own member,
 * and no object inside libc.a references any of the three (checked with
 * llvm-nm over all 847 members), so neither libc member is ever pulled in on
 * its own account.
 */

#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define WK_NSEC_PER_SEC 1000000000L

/* Sleep until `abstime`, then report a timeout.
 *
 * The deadline is interpreted against CLOCK_REALTIME. That is the same
 * assumption wasi-libc's own implementation makes — it passes _CLOCK_REALTIME
 * to clock_nanosleep unconditionally and never reads the condvar's _c_clock —
 * so this is no less correct than the function it replaces, and it avoids
 * reaching into musl's private pthread_cond_t layout to find the clock id.
 * A caller that set CLOCK_MONOTONIC via pthread_condattr_setclock would hand
 * us a small number of seconds, the subtraction below would go negative, and
 * the wait would time out early instead of late. Early is the safe direction:
 * a spurious early timeout is a legal condvar outcome that every caller
 * handles; a late one would hang the UI.
 *
 * The mutex is deliberately NOT released for the duration. POSIX requires the
 * release so that another thread can take the lock and signal; here there is
 * no other thread, so the release is unobservable, and skipping it avoids
 * touching recursive-mutex counts on a path that already cannot be woken.
 */
int pthread_cond_timedwait(pthread_cond_t *__restrict cond,
                           pthread_mutex_t *__restrict mutex,
                           const struct timespec *__restrict abstime)
{
    struct timespec now;
    struct timespec rel;

    (void)cond;
    (void)mutex;

    if (abstime == NULL || abstime->tv_nsec < 0 ||
        abstime->tv_nsec >= WK_NSEC_PER_SEC)
        return EINVAL;

    if (clock_gettime(CLOCK_REALTIME, &now) != 0)
        return ETIMEDOUT; /* no clock to compare against; nothing can signal */

    rel.tv_sec = abstime->tv_sec - now.tv_sec;
    rel.tv_nsec = abstime->tv_nsec - now.tv_nsec;
    if (rel.tv_nsec < 0) {
        rel.tv_nsec += WK_NSEC_PER_SEC;
        rel.tv_sec -= 1;
    }
    if (rel.tv_sec < 0)
        return ETIMEDOUT; /* deadline already past */

    while (rel.tv_sec > 0 || rel.tv_nsec > 0) {
        struct timespec remaining;
        /* Relative, on CLOCK_MONOTONIC: the one sleep wasip2 actually
         * implements. EINTR cannot happen (wasip2 has no signals) but the
         * loop costs nothing and keeps the function honest if it ever can.
         * Any other error means we cannot sleep at all, and there is nothing
         * better to do than report the timeout we were going to report
         * anyway. */
        int e = clock_nanosleep(CLOCK_MONOTONIC, 0, &rel, &remaining);
        if (e != EINTR)
            break;
        rel = remaining;
    }

    return ETIMEDOUT;
}

/* The untimed wait is NOT given a timeout, and NOT allowed to return.
 *
 * Reasoning, because this is the asymmetry that matters. A timed wait that
 * cannot be signalled has a correct answer: the deadline passes. An UNTIMED
 * wait that cannot be signalled has none — the caller's contract is "return
 * only when someone signals me", and in a process with one thread nobody ever
 * will. The three things we could do:
 *
 *   return 0        a spurious wakeup. Legal per POSIX, and the worst option
 *                   here: every caller of the predicate form
 *                   (`cv.wait(lk, pred)`, which is what
 *                   sal/osl/unx/conditn.cxx:124 and vcl/headless/svpinst.cxx:495
 *                   both use) loops on it, so this converts a deadlock into a
 *                   silent 100%-CPU livelock with no diagnostic at all.
 *   sleep forever   an honest deadlock, and undebuggable — the node just stops.
 *   fail loudly     the caller is a bug in the port: a wait that must be
 *                   overridden (svp's DoYield) or a code path that should
 *                   never have been reached without a thread. Crashing names
 *                   it, with a wasmtime backtrace pointing straight at it.
 *
 * So: fail loudly. wasi-libc's own stub is already a bare `unreachable`, which
 * traps with a usable backtrace; all this override adds is the sentence that
 * explains it, so whoever hits it at M4 does not have to first discover that
 * wasi-libc stubs this out. Keeping the definition here also keeps both halves
 * of the port's threading policy in one file.
 *
 * If a *reachable* untimed wait turns up, the fix is to give that call site a
 * serial path — not to soften this function.
 */
int pthread_cond_wait(pthread_cond_t *cond, pthread_mutex_t *mutex)
{
    static const char msg[] =
        "wk/libreoffice: pthread_cond_wait() on wasm32-wasip2 — this process "
        "has one thread, so this condition can never be signalled. That is a "
        "deadlock, not a timeout, so it is a crash. Give the call site above a "
        "serial path (see plugins/libreoffice/PORTING.md, \"threadless "
        "LibreOffice\").\n";

    (void)cond;
    (void)mutex;

    /* write(), not fprintf(): no stdio buffering to flush on a path that is
     * about to trap, and no stdio dependency in a shim linked into every
     * binary. */
    (void)!write(2, msg, sizeof msg - 1);
    abort();
}

/* NOT overridden here, and each omission is a decision:
 *
 *   pthread_create      wasi-libc returns ENOTSUP (58). A clean, reportable
 *                       failure — PORTING.md's policy is to leave it that way.
 *   pthread_join        wasi-libc returns 0 without touching *retval, which is
 *                       a lie, but an unobservable one: pthread_create can
 *                       never succeed, so there is never a thread to join, and
 *                       std::thread::join() checks joinable() first. Changing
 *                       it to ESRCH would only risk breaking code that checks
 *                       the return of a call it cannot legitimately make.
 *   pthread_barrier_wait / __wasilibc_futex_wait
 *                       both trap on `unreachable` exactly when they would
 *                       have to block, which is right for the same reason
 *                       pthread_cond_wait is: an unsignallable wait with no
 *                       deadline is a deadlock.
 *   pthread_mutex_lock / _timedlock, pthread_rwlock_*, pthread_key_*
 *                       verified to work uncontended, which is all a
 *                       single-threaded process can ask of them.
 *
 * A survey of all 847 members of wasi-libc's libc.a found exactly three
 * functions whose body is a bare `unreachable`: abort(), __stack_chk_fail_local()
 * and pthread_cond_wait(). There is no fourth landmine of this shape.
 */

/* ------------------------------------------------------------------------
 * Two traces, for the two failures this port keeps meeting blind.
 *
 * On wasm exception handling std::current_exception() is NULL inside a
 * std::terminate handler when terminate was reached by an exception escaping a
 * noexcept function -- verified with a five-line test case, not assumed. So the
 * usual trick of rethrowing inside the handler to print e.what() cannot work,
 * and an uncaught exception reaches the host as a bare
 *
 *     wasm trap: wasm `unreachable` instruction executed
 *
 * with no type and no message. The last place the type is still in hand is the
 * throw itself. Likewise a std::bad_alloc says nothing about who asked for how
 * much, and wasi-libc's dlmalloc does not go much past 2 GB.
 *
 * wasm-ld's --wrap (set in WASI_INTEL_GCC.mk) routes every throw and every
 * malloc in the binary -- LibreOffice's, libc++'s and all ~149 external
 * libraries' -- through here first. Both are off unless their environment
 * variable is set, because LibreOffice throws thousands of exceptions it goes
 * on to catch during an ordinary startup.
 *
 * The flags are read in a constructor rather than lazily on first use, and the
 * malloc path formats its own decimal instead of calling snprintf. Both are
 * scars: wasi-libc's getenv allocates (__wasilibc_ensure_environ), so reading
 * the environment from inside malloc is unbounded recursion, and it presents as
 * the trace being silent and the process dying somewhere else entirely.
 * ------------------------------------------------------------------------ */

static size_t wk_alloc_threshold = 0;
static int wk_trace_throw = 0;
static const char *wk_trap_throw = NULL;

__attribute__((constructor)) static void wk_trace_init(void)
{
    const char *env = getenv("WK_LO_TRACE_ALLOC");
    if (env != NULL)
        wk_alloc_threshold = (size_t)strtoull(env, NULL, 0);
    wk_trace_throw = getenv("WK_LO_TRACE_THROW") != NULL;
    wk_trap_throw = getenv("WK_LO_TRAP_THROW");
}

void __real___cxa_throw(void *thrown, void *tinfo, void (*dest)(void *));

void __wrap___cxa_throw(void *thrown, void *tinfo, void (*dest)(void *))
{
    if (wk_trace_throw && tinfo != NULL)
    {
        /* Itanium ABI: std::type_info is { vtable pointer; const char *name; },
         * so the mangled name is the second pointer-sized word. Mangled rather
         * than demangled on purpose -- __cxa_demangle allocates, and this runs
         * with an exception already in flight. */
        const char *name = ((const char *const *)tinfo)[1];
        fprintf(stderr, "wk: throw %s\n", name != NULL ? name : "(no name)");
        fflush(stderr);
    }
    if (wk_trap_throw != NULL && tinfo != NULL)
    {
        /* Abort at the throw rather than at the catch. wasmtime prints a
         * backtrace for a trap, and a thrown-and-uncaught C++ exception gives
         * one only from wherever std::terminate happened to be reached -- which
         * on this platform is a frame with nothing to do with the cause. Set
         * WK_LO_TRAP_THROW to a substring of the mangled type name and the
         * backtrace names the code that threw it. */
        const char *name = ((const char *const *)tinfo)[1];
        if (name != NULL && strstr(name, wk_trap_throw) != NULL)
        {
            fprintf(stderr, "wk: trapping at throw of %s\n", name);
            fflush(stderr);
            abort();
        }
    }
    __real___cxa_throw(thrown, tinfo, dest);
}

void *__real_malloc(size_t n);

/* Append a decimal number to a buffer, returning the new end. */
static char *wk_dec(char *p, size_t v)
{
    char tmp[24];
    char *t = tmp + sizeof tmp;
    do { *--t = (char)('0' + (v % 10)); v /= 10; } while (v != 0);
    while (t != tmp + sizeof tmp)
        *p++ = *t++;
    return p;
}

void *__wrap_malloc(size_t n)
{
    if (wk_alloc_threshold != 0 && n >= wk_alloc_threshold)
    {
        /* Hand-rolled, and write(2) rather than stdio: everything in stdio can
         * allocate, and allocating inside the allocator on the way to an
         * allocation failure is how a diagnostic becomes the crash. */
        char buf[32];
        char *p = buf + sizeof buf;
        size_t v = n;
        *--p = '\n';
        do { *--p = (char)('0' + (v % 10)); v /= 10; } while (v != 0);
        *--p = ' ';
        *--p = 'c'; *--p = 'o'; *--p = 'l'; *--p = 'l'; *--p = 'a'; *--p = 'm';
        *--p = ' '; *--p = ':'; *--p = 'k'; *--p = 'w';
        (void)!write(2, p, (size_t)(buf + sizeof buf - p));
    }
    void *p = __real_malloc(n);

    /* Always reported, with no environment variable to ask for it: an
     * allocation failure in a 32-bit address space is the end of the process,
     * and the size and the heap it failed against are the whole diagnosis.
     * Without this the caller sees only std::bad_alloc, which on this platform
     * arrives as an unnamed trap. */
    if (p == NULL && n != 0)
    {
        char buf[128];
        char *q = buf;
        const char *lead = "wk: malloc FAILED for ";
        while (*lead != '\0') *q++ = *lead++;
        q = wk_dec(q, n);
        lead = " bytes, linear memory is ";
        while (*lead != '\0') *q++ = *lead++;
        q = wk_dec(q, (size_t)__builtin_wasm_memory_size(0) * 65536u);
        lead = " bytes\n";
        while (*lead != '\0') *q++ = *lead++;
        (void)!write(2, buf, (size_t)(q - buf));
    }

    return p;
}
