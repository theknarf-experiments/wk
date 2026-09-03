/* net/if.h — the interface-configuration declarations wasm32-wasip2 has none of.
 *
 * wasi-libc ships no <net/if.h> at all, while its <sys/ioctl.h> DOES define
 * SIOCGIFCONF, SIOCGIFADDR, SIOCGIFFLAGS and friends. ACE reads those
 * constants and compiles its SIOCGIFCONF interface-enumeration path
 * (ACE::get_ip_interfaces, ACE_SOCK_Dgram::set_nic), which then has no
 * `struct ifreq` or `struct ifconf` to compile against.
 *
 * This header supplies them, with musl's layouts. It does NOT make interface
 * enumeration work, and is not meant to: wasi-libc's ioctl implements FIONREAD
 * and FIONBIO and fails everything else, so ACE asks, is refused, and reports
 * no interfaces. That is the correct answer for a wasm guest — under wk a
 * node's fabric address (10.0.0.x / fd00::x) is assigned by the host, and
 * OpenDDS is told it explicitly through `local_address`. See ../../PORTING.md,
 * "Which address am I".
 *
 * The alternative was patching the SIOCGIFCONF branch out of ACE in several
 * files. The branch is not wrong; the declarations were missing.
 *
 * On the include path via platform_wasi.GNU's -I, which comes AFTER the
 * sysroot, so a future wasi-sdk that ships a real <net/if.h> would shadow this
 * one rather than collide with it.
 */

#ifndef WK_NET_IF_H
#define WK_NET_IF_H

#include <sys/socket.h>   /* struct sockaddr */

#ifdef __cplusplus
extern "C" {
#endif

#define IF_NAMESIZE 16

/* Interface flags. Only the ones ACE tests are listed; ACE #defines any it
 * does not find itself (os_include/net/os_if.h), so this is belt and braces. */
#define IFF_UP          0x1
#define IFF_BROADCAST   0x2
#define IFF_DEBUG       0x4
#define IFF_LOOPBACK    0x8
#define IFF_POINTOPOINT 0x10
#define IFF_RUNNING     0x40
#define IFF_NOARP       0x80
#define IFF_PROMISC     0x100
#define IFF_MULTICAST   0x1000

struct if_nameindex {
    unsigned int if_index;
    char *if_name;
};

struct ifreq {
    char ifr_name[IF_NAMESIZE];
    union {
        struct sockaddr ifru_addr;
        struct sockaddr ifru_dstaddr;
        struct sockaddr ifru_broadaddr;
        struct sockaddr ifru_netmask;
        struct sockaddr ifru_hwaddr;
        short ifru_flags;
        int ifru_ivalue;
        int ifru_mtu;
        char ifru_slave[IF_NAMESIZE];
        char ifru_newname[IF_NAMESIZE];
        void *ifru_data;
    } ifr_ifru;
};

#define ifr_addr      ifr_ifru.ifru_addr
#define ifr_dstaddr   ifr_ifru.ifru_dstaddr
#define ifr_broadaddr ifr_ifru.ifru_broadaddr
#define ifr_netmask   ifr_ifru.ifru_netmask
#define ifr_hwaddr    ifr_ifru.ifru_hwaddr
#define ifr_flags     ifr_ifru.ifru_flags
#define ifr_ifindex   ifr_ifru.ifru_ivalue
#define ifr_metric    ifr_ifru.ifru_ivalue
#define ifr_mtu       ifr_ifru.ifru_mtu
#define ifr_slave     ifr_ifru.ifru_slave
#define ifr_newname   ifr_ifru.ifru_newname
#define ifr_data      ifr_ifru.ifru_data

struct ifconf {
    int ifc_len;
    union {
        void *ifcu_buf;
        struct ifreq *ifcu_req;
    } ifc_ifcu;
};

#define ifc_buf ifc_ifcu.ifcu_buf
#define ifc_req ifc_ifcu.ifcu_req

#ifdef __cplusplus
}
#endif

#endif /* WK_NET_IF_H */
