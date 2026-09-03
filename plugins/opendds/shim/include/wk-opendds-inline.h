/* wk-opendds-inline.h — the inline-runnable registry: what a thread becomes
 * when the target has no threads.
 *
 * THE PROBLEM THIS SOLVES, WHICH IS NOT THE SAME AS "BLOCKING"
 * ===========================================================
 * shim/wk-opendds-threads.c handles code that WAITS: a condition wait pumps
 * and returns as a spurious wakeup, so every `while (!pred) cv.wait()` in
 * OpenDDS becomes a correct polling loop with no upstream change at all.
 *
 * That is most of OpenDDS, but not all of it. Two places need a thread to RUN
 * A LOOP rather than to block in one:
 *
 *   OpenDDS::DCPS::DispatchService  — a ThreadPool over run_event_loop(),
 *                                     which pops queued events and fires due
 *                                     timers;
 *   OpenDDS::DCPS::ReactorTask      — one thread running
 *                                     ACE_Reactor::run_reactor_event_loop(),
 *                                     which is where the rtps_udp transport's
 *                                     socket I/O actually happens.
 *
 * Neither can be satisfied by a pumping condvar, because neither is waiting —
 * they are the things that would do the waking. And neither can simply be
 * called from the pump as-is, because both run until told to stop.
 *
 * So each is patched (see patches/opendds-0002 and -0003) to expose ONE PASS
 * of its loop, and to register that pass here when it finds it cannot spawn a
 * thread. The pump then calls every registered pass once, in registration
 * order, and that is the whole scheduler.
 *
 * WHY THE REGISTRY IS IN C, UNDER LIBC
 * ------------------------------------
 * Because wk-opendds-threads.c is what calls it, from inside
 * pthread_cond_wait, and that file must not depend on ACE or OpenDDS — it is
 * linked into everything, including C-only translation units. The registry is
 * a fixed-size array with no allocation for the same reason: it can be called
 * from a condition wait during static construction, before any allocator state
 * a caller might care about exists.
 *
 * Registration is NOT a promise that the pass will run promptly — it runs when
 * something waits, or when the node's own loop pumps. That is the correct
 * shape for a cooperative single-threaded runtime and it is why a DDS node
 * under wk must either be inside a DDS call or calling the pump; a node that
 * goes away and computes for a minute stalls its own reactor for a minute.
 */

#ifndef WK_OPENDDS_INLINE_H
#define WK_OPENDDS_INLINE_H

#ifdef __cplusplus
extern "C" {
#endif

typedef void (*wk_inline_fn) (void *arg);

/* Register one pass. Returns a slot >= 0, or -1 if the table is full (which
 * is a bug in this file's sizing, not a runtime condition worth handling —
 * OpenDDS creates a handful of these, not hundreds). */
int wk_inline_register (wk_inline_fn fn, void *arg);

/* Stop calling a registered pass. Safe to call from inside the pass itself,
 * which matters: a DispatchService can be shut down by an event it dispatched. */
void wk_inline_unregister (int slot);

/* Run every registered pass once, in registration order. Re-entrant calls are
 * ignored — see the implementation. */
void wk_inline_run_once (void);

/* How many passes are registered. The node's main loop uses this to tell
 * "nothing is running yet" from "idle". */
int wk_inline_count (void);

#ifdef __cplusplus
}
#endif

#endif /* WK_OPENDDS_INLINE_H */
