// ace-smoke.cpp — does libACE actually LINK and RUN on wasm32-wasip2?
//
// Building 307 object files proves the headers agree with wasi-libc. It proves
// nothing about whether the result loads: on this target a component is
// rejected at INSTANTIATE time if any translation unit used the wrong
// exception encoding, and a missing libc symbol only surfaces when something
// references it. Both failures land far from their cause, so they are worth
// catching in a program small enough to read.
//
// What this exercises, in the order the port needs it:
//
//   1. static construction — ACE_Object_Manager runs before main() and is
//      where a threadless ACE would abort first if it were going to;
//   2. the shim's mutexes and CONDITION VARIABLES, including a timed wait,
//      which is the port's whole threading policy in miniature;
//   3. the reactor — ACE_Select_Reactor over the fd_set that is not a bitmask
//      (see PORTING.md), including a timer, which is what OpenDDS's
//      ReactorTask drives;
//   4. a real UDP socket on wk's fabric, sent and received through
//      ACE_SOCK_Dgram's IOVEC path — i.e. through the shim's sendmsg/recvmsg,
//      which is exactly what OpenDDS's rtps_udp transport does.
//
// Run under wasmtime it needs no network permission for (1)-(3); step (4)
// talks to itself on 127.0.0.1, which wk's fabric gives every node.

#include "ace/Log_Msg.h"
#include "ace/Reactor.h"
#include "ace/Select_Reactor.h"
#include "ace/Event_Handler.h"
#include "ace/Condition_Thread_Mutex.h"
#include "ace/Thread_Mutex.h"
#include "ace/SOCK_Dgram.h"
#include "ace/INET_Addr.h"
#include "ace/OS_NS_string.h"
#include "ace/Handle_Set.h"
#include "ace/OS_NS_sys_select.h"
#include "ace/OS_NS_fcntl.h"
#include <fcntl.h>
#include "ace/Time_Value.h"
#include "ace/ACE.h"
#include "ace/OS_NS_stdlib.h"

#include <cstdio>
#include <cstring>

namespace {

int failures = 0;

void check (const char *what, bool ok)
{
  std::printf ("%-46s %s\n", what, ok ? "ok" : "FAILED");
  if (!ok) ++failures;
}

/// Fires once, from the reactor's timer queue.
class Ticker : public ACE_Event_Handler
{
public:
  int fired = 0;
  int handle_timeout (const ACE_Time_Value &, const void *) override
  {
    ++this->fired;
    return 0;
  }
};

} // namespace

int main ()
{
  // Unbuffered, because the interesting failures here are TRAPS: wasmtime
  // unwinds without running atexit, so anything still sitting in stdio's
  // buffer is lost and the log stops several checks before the actual fault.
  std::setvbuf (stdout, nullptr, _IONBF, 0);

  std::printf ("wk/opendds ACE smoke test (wasm32-wasip2)\n\n");

  // 1. Static construction got us here at all.
  check ("ACE static construction reached main", true);

  // 2. The threading shim. A mutex that cannot be contended, and a timed wait
  //    that must return ETIMEDOUT after roughly the time asked for — never
  //    instantly (that would be a 100% CPU spin in the real thing) and never
  //    never (that would be the deadlock this port exists to avoid).
  {
    ACE_Thread_Mutex m;
    check ("ACE_Thread_Mutex acquire/release", m.acquire () == 0 && m.release () == 0);

    ACE_Thread_Mutex cm;
    ACE_Condition_Thread_Mutex cv (cm);
    const ACE_Time_Value before = ACE_OS::gettimeofday ();
    ACE_Time_Value deadline = before + ACE_Time_Value (0, 50 * 1000); // 50 ms
    cv.wait (&deadline);
    const ACE_Time_Value elapsed = ACE_OS::gettimeofday () - before;
    // The shim returns spurious wakeups until the deadline passes, so one
    // call may come back early; what must NOT happen is a busy return with
    // no sleep at all. Allow generous slack: this is a "did it sleep" test.
    check ("timed condition wait slept, did not spin",
           elapsed.msec () >= 1 && elapsed.msec () < 5000);
  }

  // 3. The reactor, over the array-shaped fd_set. A timer is the part
  //    OpenDDS's ReactorTask leans on hardest.
  {
    // open() explicitly rather than relying on the constructor, so that a
    // failure is a printed check rather than a half-built reactor that traps
    // several frames later. That is not hypothetical: with a notification
    // pipe still enabled this reactor came up with no timer queue, and the
    // only symptom was `uninitialized element` inside dispatch_timer_handlers.
    std::printf ("   (ACE::max_handles() = %d, FD_SETSIZE = %d)\n",
                 (int) ACE::max_handles (), (int) FD_SETSIZE);

    ACE_Select_Reactor impl;
    // The constructor opens the reactor itself, so do NOT call open() again —
    // ACE_Select_Reactor_T::open() begins `if (this->initialized_) return -1`.
    // What tells us whether the constructor's open SUCCEEDED is the timer
    // queue: a failed open runs close(), which deletes it.
    check ("reactor opened (has a timer queue)", impl.timer_queue () != nullptr);
    if (impl.timer_queue () == nullptr)
      std::printf ("   (last error %d)\n", (int) ACE_OS::last_error ());

    ACE_Reactor reactor (&impl);

    Ticker ticker;
    const long id = reactor.schedule_timer (&ticker, nullptr, ACE_Time_Value (0, 1000));
    check ("reactor scheduled a timer", id != -1);

    // Pump for up to a second; the timer is due after 1 ms.
    for (int i = 0; i < 100 && ticker.fired == 0; ++i)
      {
        ACE_Time_Value slice (0, 10 * 1000);
        reactor.handle_events (slice);
      }
    check ("reactor dispatched the timer", ticker.fired == 1);
  }

  // 4. A UDP round trip through the shim's sendmsg/recvmsg. This is the
  //    OpenDDS rtps_udp path: a datagram gathered from several buffers, and a
  //    receive that must report the sender's address.
  {
    ACE_INET_Addr local (static_cast<u_short> (0), INADDR_LOOPBACK);
    ACE_SOCK_Dgram sock;
    const bool opened = sock.open (local) == 0;
    check ("ACE_SOCK_Dgram opened a UDP socket", opened);

    if (opened)
      {
        ACE_INET_Addr bound;
        sock.get_local_addr (bound);

        // Three buffers, one datagram — the shape of an RTPS message (header
        // plus submessages). If sendmsg were the ACE_LACKS_ stub, or if it
        // sent one datagram per iov, this would not come back as "RTPS!hello".
        char h[] = "RTPS!";
        char a[] = "hel";
        char b[] = "lo";
        iovec out[3];
        out[0].iov_base = h; out[0].iov_len = 5;
        out[1].iov_base = a; out[1].iov_len = 3;
        out[2].iov_base = b; out[2].iov_len = 2;
        const ssize_t sent = sock.send (out, 3, bound);
        check ("scatter/gather send (shim sendmsg)", sent == 10);

        char in_buf[64];
        ACE_OS::memset (in_buf, 0, sizeof in_buf);
        iovec in[1];
        in[0].iov_base = in_buf; in[0].iov_len = sizeof in_buf;
        ACE_INET_Addr from;
        // Wait for readiness SEPARATELY, then use the four-argument iovec
        // recv. ACE_SOCK_Dgram's five-argument overloads are
        //   recv (void *buf,  size_t n, ACE_Addr&, int, const ACE_Time_Value*)
        //   recv (iovec iov[], int n,   ACE_Addr&, int, ACE_INET_Addr *to_addr)
        // so passing an iovec* with a timeout silently resolves to the FIRST
        // of those -- reading one byte into the iovec array itself. (It did,
        // and returned 1: worth knowing before assuming the shim was at fault.)
        ACE_Time_Value wait (2, 0);
        const int ready = ACE::handle_read_ready (sock.get_handle (), &wait);
        check ("datagram became readable", ready == 1);
        const ssize_t got = ready == 1 ? sock.recv (in, 1, from, 0) : -1;
        if (got != 10)
          std::printf ("   (recv returned %d, errno %d, from port %d)\n",
                       (int) got, (int) ACE_OS::last_error (),
                       (int) from.get_port_number ());
        check ("gathered receive (shim recvmsg)", got == 10);
        check ("datagram arrived whole and in order",
               got == 10 && ACE_OS::strncmp (in_buf, "RTPS!hello", 10) == 0);
        check ("receive reported the sender's port",
               from.get_port_number () == bound.get_port_number ());

        sock.close ();
      }
  }

  // 5. The reactor dispatching a SOCKET, not just a timer. This is the path
  //    OpenDDS's discovery actually rides on, and it is the one that exercises
  //    ACE_Handle_Set and select() over the array-shaped fd_set -- a timer
  //    needs neither.
  {
    class Echoed : public ACE_Event_Handler
    {
    public:
      ACE_SOCK_Dgram sock;
      int reads = 0;
      ACE_HANDLE get_handle () const override { return sock.get_handle (); }
      int handle_input (ACE_HANDLE) override
      {
        char buf[64];
        ACE_INET_Addr from;
        sock.recv (buf, sizeof buf, from);
        ++reads;
        return 0;
      }
    };

    ACE_Select_Reactor impl;
    ACE_Reactor reactor (&impl);
    Echoed h;
    ACE_INET_Addr any (static_cast<u_short> (0), INADDR_LOOPBACK);
    check ("handler socket opened", h.sock.open (any) == 0);

    // The port makes every datagram socket non-blocking at open()
    // (patches/ace-0003-wasi-nonblocking-datagrams.patch). Check it here,
    // because everything below depends on it and because a regression would
    // otherwise present as a node that hangs rather than as a failed test.
    {
      const int fl = ACE_OS::fcntl (h.sock.get_handle (), F_GETFL, 0);
      check ("datagram socket is non-blocking at open",
             fl >= 0 && (fl & O_NONBLOCK) != 0);
    }

    const int reg = reactor.register_handler (&h, ACE_Event_Handler::READ_MASK);
    check ("reactor accepted a socket handler", reg == 0);

    ACE_INET_Addr bound;
    h.sock.get_local_addr (bound);
    ACE_SOCK_Dgram sender;
    ACE_INET_Addr sender_any (static_cast<u_short> (0), INADDR_LOOPBACK);
    sender.open (sender_any);
    const ssize_t put = sender.send ("ping", 4, bound);
    if (put != 4)
      std::printf ("   (send -> %d, errno %d, dest port %d)\n",
                   (int) put, (int) ACE_OS::last_error (),
                   (int) bound.get_port_number ());
    check ("datagram sent to the handler's socket", put == 4);

    // Poll the way the wk pump does -- handle_events with a ZERO timeout,
    // never a blocking one -- but over a real wall-clock budget. A tight loop
    // of N zero-timeout polls is not a test, it is a race: 200 of them finish
    // in well under a millisecond, which is less than loopback delivery takes,
    // and this check failed for exactly that reason before the sleep was
    // added. (Unbuffered printf between polls was enough to make it pass,
    // which is how the race was spotted.)
    const ACE_Time_Value deadline = ACE_OS::gettimeofday () + ACE_Time_Value (2, 0);
    while (h.reads == 0 && ACE_OS::gettimeofday () < deadline)
      {
        ACE_Time_Value zero = ACE_Time_Value::zero;
        reactor.handle_events (&zero);
        ACE_OS::sleep (ACE_Time_Value (0, 1000)); // 1 ms, as the pump sleeps
      }
    check ("reactor dispatched handle_input (poll, zero timeout)", h.reads == 1);

    // ...and then, on THIS platform, exactly once more with nothing to read.
    //
    // That is not a bug in ACE and not a bug in this port: wasip2 reports a UDP
    // socket readable until a receive finds nothing, so the empty read is what
    // CLEARS the readiness. Asserting it here pins the platform behaviour the
    // whole design rests on -- if a future wasi-sdk changes it, this check is
    // where that shows up, rather than as a mysterious extra dispatch.
    //
    // It is also why datagram sockets must be non-blocking (checked above): a
    // blocking read here would wait for a datagram that will never come, and
    // with one thread and no signals that is the end of the process. It is
    // precisely how an OpenDDS participant failed -- it announced itself once
    // and went silent, blocked in recvfrom on its SPDP socket.
    const int after_first = h.reads;
    for (int i = 0; i < 500; ++i)
      {
        ACE_Time_Value zero = ACE_Time_Value::zero;
        reactor.handle_events (&zero);
      }
    check ("drained socket is reported readable exactly once more",
           h.reads == after_first + 1);

    // The select() width. ACE rebuilds the reactor's max_handlep1_ from
    // max_set() every time a handler is REMOVED, so a handle set that does not
    // track its maximum leaves the reactor polling width 0 -- deaf, silently
    // and permanently. See patches/ace-0001-wasi-fd-set.patch.
    ACE_Handle_Set hs;
    hs.set_bit (h.sock.get_handle ());
    // max_set() answers the largest handle itself; the reactor adds the +1
    // (Select_Reactor_Base.cpp, `++this->max_handlep1_`).
    check ("handle set reports its maximum (select width)",
           hs.max_set () == h.sock.get_handle ());
    hs.clr_bit (h.sock.get_handle ());
    check ("cleared handle set reports no maximum",
           hs.max_set () == ACE_INVALID_HANDLE);

    reactor.remove_handler (&h, ACE_Event_Handler::READ_MASK | ACE_Event_Handler::DONT_CALL);
    sender.close ();
    h.sock.close ();
  }

  std::printf ("\n%s\n", failures == 0 ? "all checks passed" : "SOME CHECKS FAILED");
  return failures == 0 ? 0 : 1;
}
