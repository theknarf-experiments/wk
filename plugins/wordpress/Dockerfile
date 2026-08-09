# WordPress as a wasm container — assembled entirely by this Dockerfile, no side
# script. Builds on the `php` image (real PHP 8.2 for wasm32-wasip2, with
# pdo_sqlite), fetches WordPress + the SQLite Database Integration plugin at build
# time, drops in the SQLite adapter + config, and serves with php's built-in
# webserver over wk's fabric.
#
# Network-fetching (ADD <url>) is opt-in, so build with:
#   wk images build --network --tag wordpress plugins/wordpress/Dockerfile
# (the `php` image must be built+tagged first — see build.sh).
FROM php

# WordPress unpacks to a `wordpress/` dir under the destination; the plugin zip to
# `sqlite-database-integration/`. Both keep their names so the drop-in and WP find
# them.
ADD https://wordpress.org/latest.tar.gz /var/www
ADD https://downloads.wordpress.org/plugin/sqlite-database-integration.zip /var/www/wordpress/wp-content/plugins/

# The SQLite drop-in (wp-content/db.php) routes WordPress's DB calls to SQLite via
# the plugin; it self-locates the plugin at runtime. wp-config wires in the demo
# salts + pins the site URL to the HostPort.
COPY db.php /var/www/wordpress/wp-content/db.php
COPY wp-config.php /var/www/wordpress/wp-config.php

ENV HOME=/root
ENTRYPOINT ["/php.wasm", "-S", "0.0.0.0:8092", "-t", "/var/www/wordpress"]
