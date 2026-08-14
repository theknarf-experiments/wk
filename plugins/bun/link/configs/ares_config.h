/* Minimal c-ares config for wasm32-wasip2 (no real networking; links inert). */
#define HAVE_ARPA_INET_H 1
#define HAVE_ARPA_NAMESER_H 1
#define HAVE_ASSERT_H 1
#define HAVE_ERRNO_H 1
#define HAVE_FCNTL_H 1
#define HAVE_LIMITS_H 1
#define HAVE_NETDB_H 1
#define HAVE_NETINET_IN_H 1
#define HAVE_SIGNAL_H 1
#define HAVE_STDBOOL_H 1
#define HAVE_STRINGS_H 1
#define HAVE_STRING_H 1
#define HAVE_SYS_SOCKET_H 1
#define HAVE_SYS_TYPES_H 1
#define HAVE_SYS_STAT_H 1
#define HAVE_TIME_H 1
#define HAVE_UNISTD_H 1
#define HAVE_BOOL_T 1
#define HAVE_SSIZE_T 1
#define HAVE_STRUCT_TIMEVAL 1
#define GETHOSTNAME_TYPE_ARG2 size_t
#define RECVFROM_TYPE_ARG1 int
#define RECV_TYPE_ARG1 int
#define SEND_TYPE_ARG1 int
#define CARES_HAVE_SYS_TYPES_H 1
#define CARES_HAVE_SYS_SOCKET_H 1
#define HAVE_WRITEV 1
#define HAVE_FCNTL_O_NONBLOCK 1
#define HAVE_GETENV 1
#define HAVE_STRDUP 1
#define HAVE_STRNCMPI 0
#define HAVE_MALLOC_H 0
#define CARES_RANDOM_FILE "/dev/urandom"
#define OS "wasm32-wasip2"
#define PACKAGE_VERSION "1.34.0"
#define VERSION "1.34.0"
#define HAVE_STRUCT_SOCKADDR_IN6 1
#define HAVE_STRUCT_ADDRINFO 1
#define HAVE_STRUCT_SOCKADDR_STORAGE 1
#define HAVE_STRUCT_TIMEVAL 1
#define HAVE_GETADDRINFO 1
#define HAVE_GAI_STRERROR 1
#define HAVE_STRUCT_ADDRINFO_AI_FLAGS 1
#define CARES_HAVE_ARPA_NAMESER_H 1
#define RECVFROM_TYPE_ARG2 void *
#define RECVFROM_TYPE_ARG3 size_t
#define RECVFROM_TYPE_ARG4 int
#define RECVFROM_TYPE_ARG5 struct sockaddr *
#define RECVFROM_TYPE_ARG6 socklen_t *
#define RECVFROM_TYPE_RETV ssize_t
#define RECV_TYPE_ARG2 void *
#define RECV_TYPE_ARG3 size_t
#define RECV_TYPE_ARG4 int
#define RECV_TYPE_RETV ssize_t
#define SEND_TYPE_ARG2 const void *
#define SEND_TYPE_ARG3 size_t
#define SEND_TYPE_ARG4 int
#define SEND_TYPE_RETV ssize_t
#define HAVE_CLOCK_GETTIME_MONOTONIC 1
#define HAVE_STRNCMPI 0
#define HAVE_STRCMPI 0
