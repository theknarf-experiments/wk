// wk_dds_node.h — what a DDS participant needs to know that a wk node cannot
// find out for itself, plus the pump.
//
// Shared by publisher.cpp and subscriber.cpp. Everything specific to running
// OpenDDS inside a wk node is here; the two node programs themselves are
// ordinary DDS code.
//
// THREE THINGS A WK NODE HAS TO BE TOLD
// =====================================
//
// 1. ITS OWN ADDRESS. A wasm guest has no interface table -- wasi-libc ships an
//    <ifaddrs.h> with no getifaddrs behind it, and ACE's SIOCGIFCONF fallback
//    has nothing to answer it either -- so ACE::get_ip_interfaces() returns
//    nothing and OpenDDS cannot discover which address to advertise.
//
//    It is not asked for on the command line. Instead the node CONNECTS a UDP
//    socket to its peer and reads back getsockname(): the stack answers with
//    the local address it would send from, which is exactly the address the
//    node should advertise. No datagram is sent -- connect() on a datagram
//    socket only fixes the peer. The trick works on wk's smoltcp fabric for
//    the same reason it works on a kernel: the routing decision is what
//    assigns the local endpoint.
//
//    This matters because on the fabric a node's address (10.0.0.x) is
//    assigned by the HOST when the node is wired to a Network. It is not in
//    the .wk file and it is not the node's to choose, so hard-coding it in a
//    workspace would break the moment nodes are wired in a different order.
//
// 2. WHERE ITS PEER IS. RTPS discovery normally finds peers by UDP multicast
//    (SPDP), and wk's fabric has no multicast: a Network is a hub that routes
//    unicast IP between its members. So the peer is named on the command line
//    and reached by SpdpSendAddrs, the configuration OpenDDS's own
//    documentation recommends for any network without multicast.
//
//    The peer is given by NAME -- `--peer dds-sub` -- not by address, because
//    wk's fabric answers DNS for node names on a node's own network. That is
//    what makes an example workspace portable: names are in the .wk file,
//    addresses are not.
//
// 3. WHEN TO PUMP. See pump() below.

#ifndef WK_DDS_NODE_H
#define WK_DDS_NODE_H

#include <dds/DCPS/Service_Participant.h>
#include <dds/DCPS/Marked_Default_Qos.h>
#include <dds/DCPS/WaitSet.h>

// No dlopen on this target, so nothing can be loaded from a .conf at run time.
// These two headers are what register the RTPS discovery and the rtps_udp
// transport as static initializers instead. Forget them and the node links,
// runs, and reports "no such transport type: rtps_udp".
#include <dds/DCPS/RTPS/RtpsDiscovery.h>
#include <dds/DCPS/transport/rtps_udp/RtpsUdp.h>

#include <ace/SOCK_Dgram.h>
#include <ace/INET_Addr.h>
#include <ace/OS_NS_unistd.h>
#include <ace/OS_NS_sys_stat.h>

#include <cstdio>
#include <cstring>
#include <cstdlib>
#include <string>

namespace wk_dds {

/// The RTPS domain the demo nodes meet in. 42 is what OpenDDS's own examples
/// use; nothing here depends on the number.
const DDS::DomainId_t DOMAIN = 42;

/// The well-known SPDP multicast group, and the address a node probes to find
/// its own. RTPS fixes this group in the spec; wk's fabric carries it.
inline const char* spdp_group() { return "239.255.0.1"; }

/// SPDP's unicast port for participant 0 of a domain, by the RTPS formula
/// PB + DG*domain + d1 + PG*participant = 7400 + 250*domain + 10.
/// Each node on the fabric has an address of its own, so every node can use
/// participant id 0 and this one port -- unlike two participants sharing a
/// host, which is why the loopback tests in PORTING.md need two ports.
inline int spdp_port() { return 7400 + 250 * static_cast<int>(DOMAIN) + 10; }

struct Options {
  /// Peer node's name on the fabric (or an address), optionally `name:port`.
  ///
  /// The port is only ever needed when two participants share ONE address, as
  /// they do on loopback: OpenDDS walks participant ids so the second one
  /// binds SPDP two ports up, and each side then has to be told which. On the
  /// fabric every node has an address of its own, every node is participant 0,
  /// and the name alone is the whole configuration.
  std::string peer;
  std::string self;      ///< our own fabric address; discovered if empty
  int count = 10;        ///< publisher: how many samples to send
  bool forever = false;  ///< subscriber: read until the node is stopped
};

/// Ask the stack which address it would use to reach `peer`.
///
/// connect() on a datagram socket sends nothing; it only records the peer, and
/// in doing so makes the stack choose a route and bind a local endpoint.
/// getsockname() then reports it. Returns an empty string if the peer cannot
/// be resolved yet -- on the fabric that means the node is not wired to a
/// Network, or the peer has not started.
inline std::string local_address_towards(const std::string& peer)
{
  std::string host = peer;
  const std::string::size_type colon = host.rfind(':');
  if (colon != std::string::npos) host.erase(colon);

  ACE_INET_Addr remote;
  if (remote.set(static_cast<u_short>(spdp_port()), host.c_str()) != 0) {
    return std::string();
  }

  ACE_SOCK_Dgram sock;
  ACE_INET_Addr any(static_cast<u_short>(0), static_cast<ACE_UINT32>(INADDR_ANY));
  if (sock.open(any) != 0) {
    return std::string();
  }
  if (ACE_OS::connect(sock.get_handle(), reinterpret_cast<sockaddr*>(remote.get_addr()),
                      remote.get_size()) != 0) {
    sock.close();
    return std::string();
  }

  ACE_INET_Addr local;
  const int rc = sock.get_local_addr(local);
  sock.close();
  if (rc != 0) {
    return std::string();
  }

  char buf[64];
  if (local.get_host_addr(buf, sizeof buf) == 0) {
    return std::string();
  }
  return std::string(buf);
}

/// Write the OpenDDS configuration this node needs, and return its path.
///
/// A file rather than the ConfigStore API because it is the form every OpenDDS
/// user already knows, and because it can be read out of the node with
/// `wk attach` when something is wrong. It goes in the node's own filesystem,
/// which is private to it.
inline std::string write_config(const std::string& self)
{
  // A wk node's filesystem is its own and starts nearly empty -- an image
  // built on wk-shell has /bin, /etc and /run and nothing else, and a bare
  // plugin has less than that. So make somewhere to write rather than assuming
  // /tmp exists, and fall back to the root, which always does.
  const char* path = "/tmp/wk-dds.ini";
  ACE_OS::mkdir("/tmp", 0777);          // EEXIST is fine and is the usual case
  FILE* f = std::fopen(path, "w");
  if (!f) {
    path = "/wk-dds.ini";
    f = std::fopen(path, "w");
  }
  if (!f) {
    return std::string();
  }
  std::fprintf(f,
    "[common]\n"
    "DCPSDefaultDiscovery=wk\n"
    "DCPSGlobalTransportConfig=$file\n"
    // Told, not discovered: see (1) at the top of this file.
    "DCPSDefaultAddress=%s\n"
    "\n"
    "[rtps_discovery/wk]\n"
    // Stock RTPS discovery: SPDP announces to the well-known multicast group
    // and SEDP uses multicast too. Nothing here names a peer -- wk's fabric
    // carries multicast, so a Network behaves like the small LAN segment RTPS
    // was designed for, and any number of participants find each other with no
    // configuration at all.
    //
    // Before the fabric could do that, this had to be `SedpMulticast=0` plus
    // an explicit `SpdpSendAddrs=<peer>`, which is OpenDDS's supported answer
    // for a network without multicast -- and which meant every node had to be
    // told about every other one.
    // One second rather than the 30s default: a demo should find its peer
    // while someone is watching it.
    "ResendPeriod=1\n"
    "\n"
    "[transport/wk_rtps]\n"
    "transport_type=rtps_udp\n"
    // Port 0: the transport takes any free port and advertises it through
    // discovery. Only SPDP needs a fixed, agreed port.
    "local_address=%s:0\n",
    self.c_str(), self.c_str());
  std::fclose(f);
  return std::string(path);
}

/// Give the middleware a slice of this thread.
///
/// A DDS participant on this target runs on the caller's thread and nothing
/// else. The reactor and the event dispatcher are driven by the pump (see
/// plugins/opendds/PORTING.md, "One thread, and a condition variable that
/// pumps"), and the pump runs whenever something waits on a condition
/// variable -- which is most of the DDS API, including WaitSet::wait and
/// wait_for_acknowledgments.
///
/// A node that goes away and computes for a second therefore stalls its own
/// reactor for a second. Whenever a node idles or does work of its own outside
/// a DDS call, it should call this instead of sleeping.
inline void pump(const ACE_Time_Value& how_long)
{
  // A condition variable nobody will ever signal is exactly the pumping wait:
  // the shim runs one pass of everything registered and returns a spurious
  // wakeup, and the loop below repeats until the time is up. Using the DDS
  // API's own WaitSet keeps this honest -- it is the same call an application
  // would make, not a private back door.
  DDS::WaitSet_var ws = new DDS::WaitSet;
  DDS::ConditionSeq active;
  DDS::Duration_t d;
  d.sec = static_cast<CORBA::Long>(how_long.sec());
  d.nanosec = static_cast<CORBA::ULong>(how_long.usec() * 1000);
  ws->wait(active, d);
}

/// Parse `--peer NAME [--self ADDR] [--count N] [--forever]`, leaving every
/// other argument for OpenDDS (it takes -DCPSDebugLevel and friends).
inline bool parse(int& argc, char** argv, Options& out)
{
  for (int i = 1; i < argc; ++i) {
    const std::string a = argv[i];
    if (a == "--peer" && i + 1 < argc) { out.peer = argv[++i]; }
    else if (a == "--self" && i + 1 < argc) { out.self = argv[++i]; }
    else if (a == "--count" && i + 1 < argc) { out.count = std::atoi(argv[++i]); }
    else if (a == "--forever") { out.forever = true; }
  }
  return true;   // --peer is optional: discovery is multicast
}

/// Everything between "the node started" and "there is a DomainParticipant":
/// find our address, write the config, and hand OpenDDS the argv it expects.
/// Returns a null participant on failure, having said why.
inline DDS::DomainParticipant_var start(int argc, char** argv, Options& opt)
{
  // Which address are we? A wasm guest has no interface table (see (1) at the
  // top of this file), so ask the stack: connect a datagram socket towards
  // somewhere and read back the local endpoint it chose.
  //
  // The somewhere is the SPDP GROUP by default, which is why this node needs
  // no arguments at all -- the group is fixed by the RTPS spec and every
  // participant already talks to it. `--peer` remains accepted for the
  // loopback rehearsal, where there is no fabric and the group is not routed.
  if (opt.self.empty()) {
    opt.self = local_address_towards(opt.peer.empty() ? spdp_group() : opt.peer);
  }
  if (opt.self.empty()) {
    std::fprintf(stderr,
      "wk-dds: cannot work out this node's own address.\n"
      "        Is the node wired to a Network? (Pass --self <addr> to say it\n"
      "        outright, or --peer <node> to probe towards a named peer.)\n");
    return DDS::DomainParticipant_var();
  }

  const std::string ini = write_config(opt.self);
  if (ini.empty()) {
    std::fprintf(stderr, "wk-dds: cannot write the OpenDDS config\n");
    return DDS::DomainParticipant_var();
  }

  std::fprintf(stderr, "wk-dds: %s on domain %d, discovering via %s\n",
               opt.self.c_str(), (int)DOMAIN, spdp_group());

  // Hand OpenDDS its own argv with the config file appended. It parses and
  // removes what it understands, which is why argc/argv are taken by value
  // here and rebuilt: the node's own flags have already been read.
  static const char* kFlag = "-DCPSConfigFile";
  int dds_argc = argc + 2;
  static char** dds_argv = new char*[dds_argc + 1];
  for (int i = 0; i < argc; ++i) dds_argv[i] = argv[i];
  dds_argv[argc] = const_cast<char*>(kFlag);
  dds_argv[argc + 1] = const_cast<char*>(ini.c_str());
  dds_argv[dds_argc] = 0;

  DDS::DomainParticipantFactory_var dpf =
    TheParticipantFactoryWithArgs(dds_argc, dds_argv);
  if (!dpf) {
    std::fprintf(stderr, "wk-dds: no participant factory\n");
    return DDS::DomainParticipant_var();
  }

  DDS::DomainParticipant_var dp =
    dpf->create_participant(DOMAIN, PARTICIPANT_QOS_DEFAULT, 0,
                            OpenDDS::DCPS::DEFAULT_STATUS_MASK);
  if (!dp) {
    std::fprintf(stderr, "wk-dds: create_participant failed\n");
  }
  return dp;
}

} // namespace wk_dds

#endif // WK_DDS_NODE_H
