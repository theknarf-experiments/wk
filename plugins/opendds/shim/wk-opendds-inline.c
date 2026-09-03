/* wk-opendds-inline.c — see include/wk-opendds-inline.h for what this is for.
 *
 * A fixed-size table, no allocation, no locks (there is one thread), and one
 * re-entrancy guard. That is the entire scheduler for a single-threaded
 * OpenDDS.
 */

#include <wk-opendds-inline.h>

/* OpenDDS registers one pass per DispatchService (the global
 * ServiceEventDispatcher, plus one per transport instance that asks for its
 * own) and one per ReactorTask. A participant with a handful of transports is
 * well under ten; 32 is room to be wrong by a lot and still not care. Running
 * out is a sizing bug in this file, not a runtime condition, so the register
 * call says so rather than silently dropping a pass — a dropped pass is a
 * reactor that never runs, which presents as "discovery never completes". */
#define WK_INLINE_MAX 32

static struct {
    wk_inline_fn fn;
    void *arg;
} table[WK_INLINE_MAX];

static int in_run;

int wk_inline_register (wk_inline_fn fn, void *arg)
{
    int i;
    if (fn == 0)
        return -1;
    for (i = 0; i < WK_INLINE_MAX; i++) {
        if (table[i].fn == 0) {
            table[i].fn = fn;
            table[i].arg = arg;
            return i;
        }
    }
    return -1;
}

void wk_inline_unregister (int slot)
{
    if (slot < 0 || slot >= WK_INLINE_MAX)
        return;
    /* Clearing in place rather than compacting is what makes it safe to
     * unregister from INSIDE a pass: wk_inline_run_once walks by index and
     * re-reads the entry, so a slot that empties mid-walk is simply skipped
     * and no other slot moves under the walk. */
    table[slot].fn = 0;
    table[slot].arg = 0;
}

void wk_inline_run_once (void)
{
    int i;

    /* A pass can dispatch an event that waits on a condition variable, and
     * that wait pumps. Without this guard the second pump would re-enter every
     * pass — including the one already on the stack — and recurse until the
     * shadow stack ran out. Returning instead is correct: the outer pass is
     * already running the work, and the waiter's predicate loop will come
     * back to it. */
    if (in_run)
        return;
    in_run = 1;

    for (i = 0; i < WK_INLINE_MAX; i++) {
        /* Re-read each time: a pass may register or unregister others. */
        wk_inline_fn fn = table[i].fn;
        if (fn != 0)
            fn (table[i].arg);
    }

    in_run = 0;
}

int wk_inline_count (void)
{
    int i, n = 0;
    for (i = 0; i < WK_INLINE_MAX; i++)
        if (table[i].fn != 0)
            ++n;
    return n;
}
