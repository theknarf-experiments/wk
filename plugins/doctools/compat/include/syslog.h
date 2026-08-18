/* Stub <syslog.h> for the pdftex cross build (synctex logs through syslog on
 * platforms that have one). Everything is swallowed: a wasm guest's log is
 * its stderr, and synctex only syslogs debug chatter.
 */
#ifndef WK_DOCTOOLS_SYSLOG_H
#define WK_DOCTOOLS_SYSLOG_H

#define LOG_EMERG 0
#define LOG_ALERT 1
#define LOG_CRIT 2
#define LOG_ERR 3
#define LOG_WARNING 4
#define LOG_NOTICE 5
#define LOG_INFO 6
#define LOG_DEBUG 7

#define LOG_PID 0x01
#define LOG_CONS 0x02
#define LOG_USER (1 << 3)

#define openlog(ident, opt, facility) ((void)0)
#define closelog() ((void)0)
#define syslog(pri, ...) ((void)0)

#endif
