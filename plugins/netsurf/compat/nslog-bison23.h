/* libnslog + host bison 2.3 (the only bison on stock macOS).
 *
 * libnslog's src/Makefile already selects the right prefixing switch for
 * bison 2.3 (--name-prefix=filter_), but 2.3's generated filter-parser.h
 * declares no yyparse prototype at all — later bisons do.  src/filter.c
 * calls filter_parse(), so under clang's C99 implicit-declaration error the
 * build stops.  This header, force-included via -include, supplies the one
 * missing prototype.  It matches the definition bison emits from
 * `%parse-param { nslog_filter_t **output }` (nslog_filter_t is
 * `struct nslog_filter_s` per include/nslog/nslog.h).
 *
 * Harmless under bison >= 2.4 (the prototypes agree), and paired with the
 * build-deps.sh sed that strips the grammar's `%destructor ... <filter>`
 * block, which is bison-2.4+ syntax 2.3 cannot parse.
 */
#ifndef WK_NSLOG_BISON23_COMPAT_H
#define WK_NSLOG_BISON23_COMPAT_H

struct nslog_filter_s;
int filter_parse(struct nslog_filter_s **output);

#endif
