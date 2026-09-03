// -*- C++ -*-
//
// ACE platform configuration for wasm32-wasip2, for the wk runtime.
// Installed into $ACE_ROOT/ace/ by ../build-target.sh.
//
// This is OUR file, not a patch to upstream ACE: config-<platform>.h is ACE's
// own extension point, and WASI is a platform ACE does not ship a config for.
// Its companion is ../ace/platform_wasi.GNU.
//
// WHAT WASI IS, FOR ACE'S PURPOSES
// ================================
// wasm32-wasip2 is a POSIX-shaped target with three whole subsystems missing,
// and wasi-libc does NOT hide that fact behind failing stubs consistently — it
// DECLARES almost everything in its headers and then, per function, either
// implements it, ships nothing at all (a link error), or ships something that
// returns ENOTSUP or traps on `unreachable`. ACE's configure-by-macro model
// fits that exactly, provided every gap is named here rather than found at run
// time.
//
// The three missing subsystems:
//
//   No processes.  No fork, exec, wait, deliverable signals, process groups,
//                  uid/gid, or /etc/passwd. A wk node runs programs through
//                  wk:exec (see the repo Readme) — not something ACE_Process
//                  can drive — so ACE's process layer is off entirely.
//   No threads.    pthread_create returns ENOTSUP. ACE's threading layer is
//                  nevertheless COMPILED IN; see the note below.
//   No dynamic linking.  No dlopen, so ACE's Service Configurator cannot load
//                  a transport from a .conf at run time. Everything is static
//                  and registered by OpenDDS's static-link initializers.
//
// What DOES work, and is the whole point of the port: BSD sockets. wasi-libc's
// socket layer is real, and under wk it terminates in wk's own userspace
// smoltcp fabric rather than the host's network — so two DDS nodes wired to
// the same Network node exchange real IP packets that wk routes.
//
// HOW THE LACKS LIST WAS ESTABLISHED
// ==================================
// Not by reading documentation. Every ACE_LACKS_* below was checked twice:
//
//   1. the macro name exists in ACE's own sources (an invented LACKS macro
//      is silently ignored, which is the worst possible failure mode — it
//      looks configured and is not);
//   2. the function is genuinely absent, by `llvm-nm --defined-only` over
//      wasi-sysroot/lib/wasm32-wasip2/libc.a rather than by whether a header
//      declares it. That distinction is not academic: wasi-libc DECLARES
//      getifaddrs, semaphores, sigaction, mmap and dlopen, and defines none
//      of them in libc.
//
// ../build-target.sh re-runs check (1) before every configure.

#ifndef ACE_CONFIG_WASI_H
#define ACE_CONFIG_WASI_H
#include /**/ "ace/pre.h"

// ACE's convention: a platform config announces its platform, the way
// config-linux.h defines ACE_LINUX. patches/ace-0001-wasi-fd-set.patch reads
// this one.
#if !defined (ACE_WASI)
#  define ACE_WASI
#endif

#if !defined (ACE_PLATFORM_CONFIG)
#  define ACE_PLATFORM_CONFIG config-wasi.h
#endif

#if !defined (__ACE_INLINE__)
#  define __ACE_INLINE__
#endif

// clang is the only compiler for this target. config-g++-common.h is ACE's
// shared gcc/clang section, which config-linux.h and config-macosx.h both use.
#include "ace/config-g++-common.h"

// ---------------------------------------------------------------------------
// Threads: PRESENT AT COMPILE TIME, ABSENT AT RUN TIME
// ---------------------------------------------------------------------------
//
// wasi-libc's <unistd.h> advertises _POSIX_THREADS 200809 (verified by probe),
// so ace/config-posix.h — included at the BOTTOM of this file — defines
// ACE_HAS_THREADS and ACE_HAS_PTHREADS on its own. That is deliberately left
// alone, and it is the single most consequential decision in this file.
//
// The tempting alternative is a threadless ACE. It does not work: OpenDDS
// includes ace/Condition_Thread_Mutex.h unconditionally
// (dds/DCPS/ConditionVariable.h) and builds every queue, dispatcher and
// reactor in DCPS on ACE_Thread_Mutex + ACE_Condition, so an ACE without them
// does not survive its first object file.
//
// So ACE keeps its full pthread layer, every mutex and condition variable is a
// real libc call, and single-threaded behaviour is supplied one layer DOWN by
// ../shim/wk-opendds-threads.c on the link line: a mutex becomes a no-op
// (correct — there is nobody to contend with) and a condition wait PUMPS the
// reactor and the event dispatcher before returning as a spurious wakeup,
// which is the only thing that can let a `while (!cond) cv.wait()` loop
// terminate in a process with one thread. See ../PORTING.md, "One thread, and
// a condition variable that pumps".
//
// Three inferences config-posix.h would otherwise draw from the same
// advertised macros have to be cut off here:

// wasi-libc declares <semaphore.h> and defines none of it (no sem_init,
// sem_wait or sem_timedwait in libc.a). _POSIX_SEMAPHORES is undefined too, so
// ACE_HAS_POSIX_SEM never gets set — but say the LACKS explicitly, because the
// two ACE reads for the *named* and *unnamed* cases are separate.
#define ACE_LACKS_NAMED_POSIX_SEM
#define ACE_LACKS_UNNAMED_SEMAPHORE
#define ACE_LACKS_SEM_DESTROY
#define ACE_LACKS_SEM_UNLINK

// pthread_condattr_setclock is present in libc, but nothing reads back the
// clock it sets and the shim ignores it. Saying LACKS keeps ACE from
// advertising ACE_HAS_POSIX_MONOTONIC_CONDITIONS, which OpenDDS's
// ConditionVariable would take as a promise that a monotonic deadline is
// honoured exactly — a promise the pumping shim does not make.
#define ACE_LACKS_CONDATTR_SETCLOCK

// Nothing that only matters when a thread is created can work.
#define ACE_LACKS_PTHREAD_SETSTACK
#define ACE_LACKS_SETSCHED
#define ACE_LACKS_THREAD_PROCESS_SCOPING
#define ACE_LACKS_PTHREAD_CANCEL
#define ACE_LACKS_PTHREAD_KILL
#define ACE_LACKS_PTHREAD_SIGMASK
#define ACE_LACKS_PTHREAD_THR_SIGSETMASK
// No pthread_exit either: there is no thread to exit from, and the one that
// would call it is the process.
#define ACE_LACKS_PTHREAD_EXIT

// ---------------------------------------------------------------------------
// Processes: none at all
// ---------------------------------------------------------------------------
#define ACE_LACKS_FORK
#define ACE_LACKS_EXEC
#define ACE_LACKS_SYS_WAIT_H
#define ACE_LACKS_WAIT
#define ACE_LACKS_WAITPID
#define ACE_LACKS_SETPGID
#define ACE_LACKS_GETPGID
#define ACE_LACKS_GETPPID
#define ACE_LACKS_SETSID
#define ACE_LACKS_SYSTEM

// No user database: no getuid, no getpwnam, no <pwd.h> in the sysroot at all.
#define ACE_LACKS_PWD_H
#define ACE_LACKS_PWD_FUNCTIONS
#define ACE_LACKS_GETUID
#define ACE_LACKS_GETEUID
#define ACE_LACKS_SETUID
#define ACE_LACKS_SETEUID
#define ACE_LACKS_GETGID
#define ACE_LACKS_GETEGID
#define ACE_LACKS_SETGID
#define ACE_LACKS_SETEGID
#define ACE_LACKS_SETREUID
#define ACE_LACKS_SETREGID

// getpid does exist, from libwasi-emulated-getpid, and always answers the same
// number. ACE only uses it for log prefixes, which is harmless — but it is NOT
// a real process id, so nothing may key identity off it. That matters for
// OpenDDS specifically: an RTPS participant GUID is derived from pid + host by
// default, and here every node would derive the SAME one. The demo nodes
// therefore set a GuidInterface explicitly; see ../PORTING.md, "Two nodes, two
// GUIDs".

// ---------------------------------------------------------------------------
// Signals: the header exists, delivery does not
// ---------------------------------------------------------------------------
//
// wasi-libc advertises _POSIX_REALTIME_SIGNALS 200809 and it is not true:
// nothing can raise a signal at a wasm guest, and libc.a defines no sigaction,
// sigprocmask, sigwait, kill or alarm. Undo the advertisement before
// config-posix.h can act on it, and switch off ACE's signal dispatch — a
// reactor left waiting on a signal-based wakeup waits for something that
// cannot arrive.
#undef ACE_HAS_POSIX_REALTIME_SIGNALS
#define ACE_LACKS_UNIX_SIGNALS
#define ACE_LACKS_SIGACTION
#define ACE_LACKS_SIGSET
#define ACE_LACKS_SIGPROCMASK
#define ACE_LACKS_SIGWAIT
#define ACE_LACKS_SIGNAL
#define ACE_LACKS_KILL
#define ACE_LACKS_ALARM
#define ACE_LACKS_SIGINFO_H
#define ACE_LACKS_UCONTEXT_H

// POSIX asynchronous I/O: no <aio.h>, no aio_* anywhere. Without this ACE
// builds its Proactor against them. (config-posix.h only sets
// ACE_HAS_AIO_CALLS from _POSIX_ASYNCHRONOUS_IO, which wasi-libc does not
// define — the #undef is belt and braces for a future sysroot that does.)
#undef ACE_HAS_AIO_CALLS
#define ACE_LACKS_AIO_H

// ---------------------------------------------------------------------------
// Dynamic linking: none
// ---------------------------------------------------------------------------
//
// wasm32-wasip2 has no shared objects and no dlopen (libdl.a is empty stubs).
// The consequence is architectural, not cosmetic: ACE's Service Configurator
// normally finds a DDS transport or discovery implementation by dlopen()ing it
// from a .conf file at run time. Here everything is linked in, and OpenDDS's
// static-link initializers (dds/DCPS/StaticIncludes.h, and the
// OpenDDS_Rtps/OpenDDS_Rtps_Udp library initializers) are what register them.
// A node that forgets to include those links fine and then reports "no such
// transport type: rtps_udp" at run time.
#define ACE_LACKS_DLFCN_H
#define ACE_LACKS_DLCLOSE
#define ACE_HAS_DYNAMIC_LINKING 0

// ---------------------------------------------------------------------------
// System V IPC, shared memory, mmap
// ---------------------------------------------------------------------------
//
// None of it exists. libwasi-emulated-mman does provide an mmap, but only for
// anonymous mappings — it cannot map a file and there is no second process to
// share with — so ACE_Mem_Map and ACE's position-independent allocators are
// off. This is also why the port builds the rtps_udp transport and not
// `shmem`: a shared-memory transport needs shared memory and a second process,
// and there is neither. rtps_udp is the one that matters anyway — it is what
// puts real packets on wk's fabric.
#define ACE_LACKS_MMAP
#define ACE_LACKS_MPROTECT
#define ACE_LACKS_MSYNC
#define ACE_LACKS_MADVISE
#define ACE_LACKS_SYSV_SHMEM
#define ACE_LACKS_SYS_IPC_H
#define ACE_LACKS_SYS_SHM_H
#define ACE_LACKS_SYS_SEM_H
#define ACE_LACKS_SYS_MSG_H
#define ACE_LACKS_SEMBUF_T
#define ACE_LACKS_SYSV_MSQ_PROTOS
#undef ACE_HAS_SHM_OPEN
#define ACE_LACKS_SHM_UNLINK

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------
//
// wk gives a node a real filesystem of its own (the vfs), with real symlinks,
// so most of this works and is NOT listed here: access, chdir, getcwd,
// readlink, realpath, chmod and fchmod are all present in libc.a. What is
// missing is anything about ownership or temporary-file creation.
#define ACE_LACKS_UMASK
#define ACE_LACKS_MKFIFO
#define ACE_LACKS_MKTEMP
#define ACE_LACKS_MKSTEMP
// No STREAMS: no <stropts.h>, and so no `struct strrecvfd` for the
// descriptor-passing that ACE_SPIPE_Stream does over it.
#define ACE_LACKS_STROPTS_H
#define ACE_LACKS_STRRECVFD
#define ACE_LACKS_SYS_SYSCTL_H
// No syslog daemon to log to: a node's output goes to its own terminal, which
// `wk attach` shows. Both halves are needed — the first keeps ACE from
// including <syslog.h>, the second compiles out ACE_Log_Msg_UNIX_Syslog, the
// backend that would call into it.
#define ACE_LACKS_SYSLOG_H
#define ACE_LACKS_UNIX_SYSLOG
#define ACE_LACKS_TERMIOS_H
#define ACE_LACKS_RLIMIT
#define ACE_LACKS_GETLOADAVG

// ---------------------------------------------------------------------------
// Sockets: the part that works, and the reason this port exists
// ---------------------------------------------------------------------------
#define ACE_HAS_SOCKLEN_T
#define ACE_HAS_POLL

// select() takes a NON-const `struct timeval *` here, as it does on most
// platforms; ACE otherwise assumes const and passes a `const timeval *` that
// will not convert.
#define ACE_HAS_NONCONST_SELECT_TIMEVAL

// fd_set IS NOT A BITMASK on this target, and that is the single most
// surprising thing about wasi-libc's socket layer. It is
//    typedef struct { size_t __nfds; int __fds[FD_SETSIZE]; } fd_set;
// — a count and an array of descriptors, the SHAPE Win32 uses — and there is
// no fd_mask, no NFDBITS and no fds_bits member anywhere in the sysroot. ACE's
// ACE_Handle_Set has exactly this second mode already (Win32's), so the port
// takes it and names the two members, via the three-line extension point that
// patches/ace-0001-wasi-fd-set.patch adds. Nothing here emulates a bitmask:
// FD_SET/FD_CLR/FD_ISSET are wasi-libc's own, and only ACE's iterator, its
// num_set() and its dump() ever look inside.
#define ACE_FD_SET_COUNT __nfds
#define ACE_FD_SET_ARRAY __fds

// No socketpair (wasi-libc's declaration is inside an
// `#ifdef __wasilibc_unmodified_upstream`, i.e. compiled out). ACE uses it for
// ACE_Pipe, which backs the Select Reactor's NOTIFICATION PIPE; without it ACE
// falls back to ::pipe, and wasi-libc's pipe is ENOSYS.
#define ACE_LACKS_SOCKETPAIR

// So: no notification pipe, for every reactor in ACE, TAO and OpenDDS.
//
// This is not a workaround, it is the correct configuration for this port. The
// notification pipe exists so that ANOTHER THREAD can wake a reactor blocked
// in select(); with one thread there is no other thread, and the pump
// (../shim/wk-opendds-threads.c) is what advances the reactor instead.
//
// It has to be said here rather than at each construction site because
// ACE_Select_Reactor opens the pipe in its CONSTRUCTOR, and a failure there
// leaves the reactor half-built — with, in particular, no timer queue. The
// symptom is not an error but `wasm trap: uninitialized element` inside
// ACE_Select_Reactor_T::dispatch_timer_handlers, i.e. an indirect call through
// a vtable pointer that was never set, several calls away from the cause. That
// trap is exactly what the smoke test hit before this line existed.
//
// ACE's own config-mqx.h and config-posix-nonetworking.h set it for the same
// reason.
#define ACE_DISABLE_NOTIFY_PIPE_DEFAULT 1

// wasi-libc ships no <net/if.h>, so ../shim/include/net/if.h supplies one —
// struct ifreq, struct ifconf and struct if_nameindex, which ACE's SIOCGIFCONF
// paths need in order to compile. ACE_LACKS_NET_IF_H is therefore NOT set:
// there is a header to include, and ACE's own placeholder
// (ACE_LACKS_STRUCT_IF_NAMEINDEX) would collide with the real definition.
// The FUNCTIONS remain absent — libc defines neither if_nametoindex nor
// if_nameindex — and enumeration fails at run time, which is the truth.
#define ACE_LACKS_IF_NAMEINDEX

// sendmsg/recvmsg are DECLARED by <sys/socket.h> and defined nowhere in libc.
// ACE_SOCK_Dgram's scatter/gather send — which is what OpenDDS's rtps_udp
// transport uses on every datagram, because an RTPS message is a header plus N
// submessages in separate buffers — is exactly that call. ../shim/wk-opendds-net.c
// supplies both over sendto/recvfrom, the same way plugins/bun supplies
// sendmmsg/recvmmsg for node:dgram. So ACE may keep believing in them:
#define ACE_HAS_4_4BSD_SENDMSG_RECVMSG

// No AF_UNIX: <sys/un.h> is not in the sysroot, so ACE_LSOCK and ACE_UNIX_Addr
// have nothing to build on.
#define ACE_LACKS_UNIX_DOMAIN_SOCKETS

// No raw sockets, so no ICMP and none of ACE's ping support.
#undef ACE_HAS_ICMP_SUPPORT

// No interface table to enumerate. wasi-libc ships an <ifaddrs.h> with no
// getifaddrs behind it, and ACE's fallback — SIOCGIFCONF through ioctl — has
// nothing to answer it either. So ACE::get_ip_interfaces() returns nothing,
// and OpenDDS must be TOLD its address rather than discovering it. Under wk
// that is the honest model anyway: a node's fabric address (10.0.0.x /
// fd00::x) is assigned by the host, not owned by the guest. See ../PORTING.md,
// "Which address am I".
#undef ACE_HAS_GETIFADDRS
#undef ACE_HAS_NETLINK
#define ACE_LACKS_IF_NAMETOINDEX

// Name resolution works: wk's fabric answers DNS for node names on a node's
// own network — that is what makes `http://www:8000/` resolve from another
// node — and wasi-libc's getaddrinfo/gethostbyname reach it. Only the
// reentrant _r variants and the IPv6 getipnode* family are missing.
#define ACE_LACKS_NETDB_REENTRANT_FUNCTIONS
#define ACE_LACKS_GETIPNODEBYNAME
#define ACE_LACKS_GETIPNODEBYADDR

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------
//
// CLOCK_MONOTONIC works, and gettimeofday and clock_nanosleep are both real.
// The wasip2 quirk to know about is that clock_nanosleep on CLOCK_REALTIME
// returns ENOTSUP while CLOCK_MONOTONIC works — documented at length in
// plugins/libreoffice/shim/wk-wasi-threads.c. Nothing here sleeps on the
// realtime clock, and ../shim/wk-opendds-threads.c uses the monotonic one.
#define ACE_HAS_CLOCK_GETTIME
#define ACE_HAS_CLOCK_GETTIME_MONOTONIC
#undef ACE_HAS_UALARM

// wasi-libc's gettimeofday is the two-argument POSIX one,
//   int gettimeofday (struct timeval *restrict, void *restrict);
// where the second argument is the vestigial timezone pointer. ACE calls the
// ONE-argument form unless told otherwise, and this is the macro that says
// "second argument, typed void *".
#define ACE_HAS_VOIDPTR_GETTIMEOFDAY

// ---------------------------------------------------------------------------
// Types wasi-libc DOES define, which ACE would otherwise define again
// ---------------------------------------------------------------------------
//
// The other half of the configuration, and the half that is easy to overlook:
// ACE's ace/os_include/ headers each carry a fallback definition of a POSIX
// type for platforms that lack it, guarded by ACE_HAS_<TYPE>. Saying nothing
// is not neutral — it means "define it yourself", and against a libc that
// already has the type the result is a hard redefinition error, not a silent
// mismatch. All four of these were found exactly that way on the first ACE
// build.
#define ACE_HAS_POSIX_TIME    // struct timespec  (os_include/os_time.h)
#define ACE_HAS_MSG           // struct msghdr    (os_include/sys/os_socket.h)
#define ACE_HAS_SSIZE_T       // ssize_t          (os_include/sys/os_types.h)
#define ACE_HAS_CPU_SET_T     // cpu_set_t        (os_include/os_sched.h)
#define ACE_HAS_SIG_ATOMIC_T  // sig_atomic_t     (os_include/os_signal.h)
#define ACE_HAS_DIRENT        // DIR / struct dirent (os_include/os_dirent.h)

// O_NONBLOCK, not the ancient O_NDELAY, is what wasi-libc's <fcntl.h> has —
// and ACE_NONBLOCK falls back to O_NDELAY without this.
#define ACE_HAS_POSIX_NONBLOCK

// No time zone database and no tzset().
#define ACE_LACKS_TZSET

// timespec is the one that needs BOTH halves, and the pairing is not obvious.
// ACE_HAS_POSIX_TIME above suppresses os_time.h's fallback
//   typedef struct timespec { ... } timespec_t;
// which was defining the STRUCT (a redefinition here) and the TYPEDEF NAME (which
// nothing else provides — musl and wasi-libc have `struct timespec` but no
// `timespec_t`). ACE_LACKS_TIMESPEC_T is what asks for just the typedef,
// `typedef struct timespec timespec_t;`. Set only the first and ACE compiles
// until the first of ~60 uses of timespec_t; set only the second and the
// struct clashes with the sysroot's.
#define ACE_LACKS_TIMESPEC_T

// pthread_rwlock_t and the pthread_rwlock_* family are both declared and
// DEFINED in wasi-libc (checked with llvm-nm), so ACE can use the UNIX98
// reader/writer lock directly instead of looking for Solaris's <synch.h>.
// Without this ACE_rwlock_t has no definition at all and ~64 uses fail.
#define ACE_HAS_PTHREADS_UNIX98_EXT

// ---------------------------------------------------------------------------
// Target facts and the C++ library
// ---------------------------------------------------------------------------
//
// wasm32 is ILP32 little-endian with a 64-bit long long — and, unusually for
// an ILP32 target, 8-byte-aligned doubles and no unaligned-access penalty.
#define ACE_SIZEOF_LONG 4
#define ACE_SIZEOF_VOID_P 4
#define ACE_HAS_STANDARD_CPP_LIBRARY 1
#define ACE_HAS_STRING_CLASS

// Wide characters exist and are 4 bytes. Both halves matter, and the failure
// if they are omitted is well downstream of anything about wchar: without
// ACE_HAS_WCHAR, ACE_WCHAR_T falls back to ACE_UINT16 while ACE's own CDR code
// keeps using std::wstring, so CDR_Stream.cpp fails to compile on
// `unsigned short *` vs `wchar_t *`. ACE cannot infer the size (there is no
// portable way, and its own comment says so): the fallback guess is 0, which
// is deliberately wrong so that Basic_Types_Test catches it. wasm32's wchar_t
// is `int`.
#define ACE_HAS_WCHAR
#define ACE_SIZEOF_WCHAR 4

// ...and the wide-string calls ACE reaches for that this libc spells
// differently or not at all. `wcstok` here is the reentrant POSIX three-argument
// form, not the two-argument C89 one ACE defaults to; the other three are
// Microsoft names with no POSIX equivalent, for which ACE has its own
// emulations.
#define ACE_HAS_3_PARAM_WCSTOK
#define ACE_LACKS_ITOW
#define ACE_LACKS_WCSICMP
#define ACE_LACKS_WCSNICMP

// musl-derived libcs have no isctype().
#define ACE_LACKS_ISCTYPE
#define ACE_HAS_STRNLEN
#define ACE_HAS_WCSNLEN
// strerror_r exists in the XSI flavour — `int strerror_r(int, char *, size_t)`
// — not the GNU one that returns char *. ACE assumes GNU unless told.
#define ACE_HAS_STRERROR_R
#define ACE_HAS_STRERROR_R_XSI

// Pull in the POSIX feature detection LAST, so everything above wins: every
// macro it sets is guarded by !defined(ACE_HAS_*) / !defined(ACE_LACKS_*).
#include "ace/config-posix.h"

// ...with ONE exception, which is why this section is below the include and
// not above it. config-posix.h sets ACE_HAS_POSIX_REALTIME_SIGNALS from
// _POSIX_REALTIME_SIGNALS with NO "lacks" guard to head it off, so the #undef
// has to come after. wasi-libc advertises _POSIX_REALTIME_SIGNALS 200809 and
// there is no SIGRTMIN behind it; left set, ACE's
//    #define ACE_SIGRTMIN SIGRTMIN
// (os_include/os_signal.h) fails to compile.
#undef ACE_HAS_POSIX_REALTIME_SIGNALS

// ---------------------------------------------------------------------------
// What wasi-libc declares nowhere
// ---------------------------------------------------------------------------
//
// struct cmsghdr, the whole CMSG_* accessor family, and the sendmsg/recvmsg
// prototypes all sit inside `#ifdef __wasilibc_unmodified_upstream` in
// wasi-libc's <sys/socket.h> — never defined, so they are not merely
// unimplemented but undeclared — while <netinet/in.h> still defines IP_PKTINFO
// and <sys/ioctl.h> still defines SIOCGIFCONF. ACE reads those constants,
// compiles the matching code, and has no types to compile it against.
//
// ../shim/include/ supplies the declarations rather than patching the branches
// out of ACE, which would take diffs in several files and would be the wrong
// shape — the branches are not wrong, the declarations are missing. The header
// itself explains what each one then does at run time (in short: the CMSG walk
// runs zero times, and interface enumeration reports nothing, both of which
// are the right answers here).
//
// It is included from this file because ace/config.h includes THIS file at the
// top of every ACE, TAO and OpenDDS translation unit — exactly the reach the
// call sites have.
#include <wk-wasi-net-compat.h>

// sendmsg/recvmsg must NOT be declared "lacking": ACE_LACKS_SENDMSG and
// ACE_LACKS_RECVMSG turn ACE_OS::sendmsg/recvmsg into ACE_NOTSUP_RETURN (-1)
// with no emulation, and OpenDDS's rtps_udp transport is built on exactly that
// pair — an RTPS message is a header plus N submessages in separate buffers,
// and every receive needs the sender's address alongside the payload. Stub
// them and the transport links, runs, and moves no data at all.
// ../shim/wk-opendds-net.c implements both over sendto/recvfrom, the shape
// plugins/bun uses for sendmmsg/recvmmsg on the node:dgram path.

#include /**/ "ace/post.h"
#endif /* ACE_CONFIG_WASI_H */
