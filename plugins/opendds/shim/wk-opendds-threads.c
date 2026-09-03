/* wk-opendds-threads.c — the threading policy for OpenDDS on wasm32-wasip2.
 *
 * The port's whole answer to "OpenDDS is built on threads and this target has
 * none" lives in this one file on the link line, rather than in #ifdefs spread
 * across ACE, TAO and OpenDDS. Same placement, and the same reasoning, as
 * plugins/libreoffice/shim/wk-wasi-threads.c — but NOT the same semantics, and
 * the difference is the interesting part.
 *
 * WHY LIBREOFFICE'S POLICY DOES NOT WORK HERE
 * ===========================================
 * LibreOffice's shim says: a timed wait times out, an untimed wait is a bug in
 * the port and crashes loudly. That works because Impress genuinely does not
 * use threads — a reachable untimed wait there means something was mis-ported.
 *
 * OpenDDS is the opposite. Its event architecture IS threads:
 *
 *   ReactorTask::open_reactor_task()  spawns a thread to run
 *                                     ACE_Reactor::run_reactor_event_loop()
 *   DispatchService                   runs a ThreadPool over run_event_loop()
 *   WaitSet::wait, wait_for_acknowledgments, and discovery settling
 *                                     block on condition variables that only
 *                                     those threads can ever signal
 *
 * Crash on every untimed wait and a participant cannot finish being created.
 *
 * THE IDEA: A CONDITION WAIT THAT PUMPS
 * =====================================
 * In a process with ONE thread, the only thing that can ever make
 *
 *     while (!condition) cv.wait (lock);
 *
 * terminate is running the work that would have signalled it. So that is what
 * the wait does: it runs one pass of the reactor and drains the event
 * dispatcher, then returns as a spurious wakeup. A spurious wakeup is legal
 * POSIX, every OpenDDS wait is already written as a predicate loop, and so
 * each of those loops becomes a correct polling loop with NO change to OpenDDS.
 *
 * That is the entire trick, and it is why this port needs two small OpenDDS
 * patches (for the threads that must RUN A LOOP rather than merely block)
 * instead of a rewrite.
 *
 * WHAT "PUMP" MEANS
 * -----------------
 * This file is C, sits under libc, and is linked into everything, so it must
 * not depend on ACE or OpenDDS. Pumping is therefore delegated to the inline
 * runnable registry (wk-opendds-inline.c): the two OpenDDS components that
 * need a thread to RUN A LOOP rather than to block in one — DispatchService
 * and ReactorTask — register one pass of their loop there when they find they
 * cannot spawn. Pumping is running every registered pass once.
 *
 * Before a participant exists, nothing is registered and nothing can make
 * progress; see "nothing registered" below.
 *
 * MUTEXES
 * -------
 * pthread_mutex_lock/unlock become no-ops that succeed. This is not an
 * approximation: with one thread there is nobody to contend with, so no lock
 * can ever be held by anyone else, and every lock succeeds immediately in
 * reality too. Recursive mutexes need no counting for the same reason.
 *
 * Not overridden, deliberately: pthread_create (wasi-libc's already returns a
 * clean ENOTSUP, which is what the OpenDDS patches check) and pthread_join
 * (returns 0 without touching *retval — a lie, but unobservable, since no
 * thread can ever have been created to join).
 *
 * ARCHIVE ORDER
 * -------------
 * lld fetches the FIRST archive member that offers a name, so being the strong
 * definition does not help if libc offered a weak one first. The link line
 * therefore uses -Wl,--whole-archive around libwkopendds.a (see
 * ../build-shim.sh), which force-includes these objects before any reference
 * is resolved. plugins/libreoffice's shim documents the same trap at length,
 * including the verification both ways.
 */

#include <errno.h>
#include <pthread.h>
#include <wk-opendds-inline.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define WK_NSEC_PER_SEC 1000000000L

/* How long a pumping untimed wait may make no progress before we call it a
 * deadlock. Generous: RTPS discovery has a default resend period of 30s, and
 * a subscriber legitimately waits a long time for a publisher that has not
 * started yet. This is a watchdog for "the pump cannot possibly satisfy this",
 * not a timeout for "the network is slow". */
#define WK_PUMP_DEADLOCK_SECONDS 300

/* How long to sleep between pump passes when the pump reports nothing to do.
 * Without this an idle participant is a 100%-CPU spin — the same trap
 * plugins/libreoffice's shim documents for an instantly-returning timed wait.
 * 1 ms keeps latency well under any DDS deadline that matters. */
#define WK_PUMP_IDLE_NSEC (1000L * 1000L)

/* Re-entrancy is handled inside wk_inline_run_once(), which ignores a nested
 * call — a pass may dispatch an event that waits, and that wait pumps. This
 * flag exists for a different reason: the wait functions below need to know
 * whether they are ALREADY inside a pump, so they can return at once instead
 * of pumping into a no-op and then sleeping for no reason. */
static int wk_pumping;

/* WK_DDS_TRACE=1 in the environment reports how often the pump actually runs.
 * "The pump is registered" and "the pump is being called" are different
 * claims, and only the second one explains a participant that comes up, sends
 * its first SPDP announcement, and then goes quiet. */
static long wk_pump_passes;
static int wk_trace = -1;

static void wk_pump_once (void)
{
    if (wk_pumping)
        return;
    if (wk_trace < 0)
        wk_trace = getenv ("WK_DDS_TRACE") != 0;
    wk_pumping = 1;
    wk_inline_run_once ();
    wk_pumping = 0;
    ++wk_pump_passes;
    if (wk_trace && (wk_pump_passes <= 3 || wk_pump_passes % 2000 == 0)) {
        char buf[64];
        int n = snprintf (buf, sizeof buf, "[wk] pump pass %ld (registered %d)\n",
                          wk_pump_passes, wk_inline_count ());
        (void)!write (2, buf, (size_t) n);
    }
}

static void wk_idle_sleep (void)
{
    /* CLOCK_MONOTONIC, relative: the one sleep wasip2 actually implements.
     * CLOCK_REALTIME returns ENOTSUP immediately here — the wasip2 quirk
     * plugins/libreoffice/shim/wk-wasi-threads.c documents in full. */
    struct timespec rel;
    rel.tv_sec = 0;
    rel.tv_nsec = WK_PUMP_IDLE_NSEC;
    clock_nanosleep (CLOCK_MONOTONIC, 0, &rel, 0);
}

static void wk_die (const char *msg)
{
    /* write(), not fprintf(): no stdio buffering to flush on a path that is
     * about to trap, and no stdio dependency in a shim linked into every
     * binary. */
    (void)!write (2, msg, strlen (msg));
    abort ();
}

/* --- mutexes: no-ops that succeed ---------------------------------------- */

int pthread_mutex_lock (pthread_mutex_t *m)    { (void)m; return 0; }
int pthread_mutex_trylock (pthread_mutex_t *m) { (void)m; return 0; }
int pthread_mutex_unlock (pthread_mutex_t *m)  { (void)m; return 0; }

/* --- the untimed wait: pump until the predicate can be satisfied ---------- */

static long wk_untimed_waits, wk_timed_waits;

int pthread_cond_wait (pthread_cond_t *cond, pthread_mutex_t *mutex)
{
    ++wk_untimed_waits;
    if (wk_trace > 0 && wk_untimed_waits <= 10) {
        char b3[64];
        int n3 = snprintf (b3, sizeof b3, "[wk] untimed wait #%ld\n", wk_untimed_waits);
        (void)!write (2, b3, (size_t) n3);
    }
    (void)cond;
    /* The mutex is deliberately NOT released. POSIX requires it so another
     * thread can take the lock and signal; there is no other thread, the
     * mutex operations above are no-ops anyway, and so the release is
     * unobservable. */
    (void)mutex;

    if (wk_pumping) {
        /* Nested: the outer pump is already running the work this wait is
         * waiting for. Return as a spurious wakeup and let the caller's
         * predicate loop come back to us. */
        return 0;
    }

    if (wk_inline_count () == 0) {
        /* Nothing is registered, so nothing in this process can ever signal
         * this condition. That is a deadlock, not a slow wait, and the honest
         * thing is a backtrace pointing at the call site. */
        wk_die ("wk/opendds: pthread_cond_wait() with nothing registered to "
                "pump — this process has one thread, so nothing can signal "
                "this condition. Whatever is above this frame needed a thread "
                "to run a loop and did not register an inline pass for it (see "
                "plugins/opendds/PORTING.md, \"One thread, and a condition "
                "variable that pumps\").\n");
    }

    /* One pass, then return as a spurious wakeup so the caller can re-test its
     * predicate — the caller is the only party that knows whether the pump
     * achieved anything. Looping here instead would be equivalent but would
     * hide the progress question inside a function that cannot answer it. */
    wk_pump_once ();
    return 0;
}

/* --- the timed wait ------------------------------------------------------- */

int pthread_cond_timedwait (pthread_cond_t *__restrict cond,
                            pthread_mutex_t *__restrict mutex,
                            const struct timespec *__restrict abstime)
{
    struct timespec now;

    (void)cond;
    (void)mutex;

    ++wk_timed_waits;
    if (wk_trace > 0 && wk_timed_waits <= 30) {
        struct timespec nw;
        clock_gettime (CLOCK_REALTIME, &nw);
        char b2[160];
        int n2 = snprintf (b2, sizeof b2,
                           "[wk] timedwait #%ld untimed=%ld abstime=%lld.%09ld now(RT)=%lld.%09ld\n",
                           wk_timed_waits, wk_untimed_waits,
                           abstime ? (long long) abstime->tv_sec : -1,
                           abstime ? abstime->tv_nsec : 0,
                           (long long) nw.tv_sec, nw.tv_nsec);
        (void)!write (2, b2, (size_t) n2);
    }

    if (abstime == NULL || abstime->tv_nsec < 0 ||
        abstime->tv_nsec >= WK_NSEC_PER_SEC)
        return EINVAL;

    if (wk_pumping)
        return 0; /* nested: spurious wakeup, as above */

    /* One pump pass, then decide. Doing the pass FIRST matters: a caller that
     * asks for a zero or already-past deadline (OpenDDS does, to poll) should
     * still get one unit of work done rather than an instant ETIMEDOUT. */
    wk_pump_once ();

    if (clock_gettime (CLOCK_REALTIME, &now) != 0)
        return ETIMEDOUT;

    if (now.tv_sec > abstime->tv_sec ||
        (now.tv_sec == abstime->tv_sec && now.tv_nsec >= abstime->tv_nsec))
        return ETIMEDOUT;

    /* Deadline still ahead. Sleep a short slice and return as a spurious
     * wakeup, so the caller re-tests its predicate and comes back. Returning
     * IMMEDIATELY would be legal and would turn every idle DDS participant
     * into a 100%-CPU spin; sleeping the WHOLE remaining time would make the
     * pump miss everything that arrives in between, which for a 30-second RTPS
     * resend period is most of it.
     *
     * The sleep is UNCONDITIONAL — in particular it does not depend on a pump
     * being registered. Guarding it on the hook was the first version and it
     * was wrong in the one case that is easiest to hit: before a participant
     * exists nothing is registered, so every timed wait returned in zero time
     * and the spin was back. The smoke test caught it as "timed condition
     * wait slept, did not spin: FAILED". */
    wk_idle_sleep ();

    return 0;
}

/* The deadlock watchdog is intentionally NOT armed inside pthread_cond_wait
 * above: that function does one pump pass and returns, so it never spins on
 * its own — the spinning, if any, is in the CALLER's predicate loop, where a
 * watchdog here could not see it. If a livelock shows up in practice, the
 * place to arm it is the pump itself (count consecutive passes that dispatch
 * nothing), and WK_PUMP_DEADLOCK_SECONDS above is the budget to use.
 * Recorded here rather than deleted so the next person does not have to
 * rediscover why the obvious place is the wrong one. */
