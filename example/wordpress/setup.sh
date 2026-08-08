#!/usr/bin/env bash
# Fetch + configure WordPress to run on wk's PHP container with a SQLite database.
# WordPress and its data are large and machine-specific, so they're NOT tracked;
# this script materializes them under app/ (gitignored). Then:
#   wk run example/wordpress.wk        # and visit http://localhost:8092
# The first visit runs WordPress's 5-minute install (writes the SQLite DB into
# app/wp-content/database/, on the writable bind mount).
#
# A real MySQL/MariaDB server can't feasibly run as wasm, so this uses SQLite via
# WordPress's official SQLite Database Integration plugin (a wp-content/db.php
# drop-in) — the same database wk's PHP build links (pdo_sqlite).
set -euo pipefail
cd "$(dirname "$0")"

APP=app
PORT="${WP_PORT:-8092}"
WP_URL="http://localhost:$PORT"

if [ -f "$APP/wp-load.php" ]; then
    echo "WordPress already set up in $(pwd)/$APP (delete it to re-fetch)."
    exit 0
fi

echo "fetching WordPress..."
curl -fsSL https://wordpress.org/latest.tar.gz -o wp.tar.gz
rm -rf "$APP" && mkdir -p "$APP"
tar xzf wp.tar.gz --strip-components=1 -C "$APP"
rm -f wp.tar.gz

echo "fetching the SQLite Database Integration plugin..."
curl -fsSL https://downloads.wordpress.org/plugin/sqlite-database-integration.zip -o sqlite-di.zip
unzip -oq sqlite-di.zip -d "$APP/wp-content/plugins/"
rm -f sqlite-di.zip

# The SQLite drop-in: db.copy self-locates the plugin (realpath fallback), so it
# works unmodified regardless of the in-container mount path.
cp "$APP/wp-content/plugins/sqlite-database-integration/db.copy" "$APP/wp-content/db.php"

echo "writing wp-config.php..."
cat > "$APP/wp-config.php" <<PHP
<?php
// wk demo WordPress on SQLite. The MySQL constants are unused (the db.php
// drop-in routes to SQLite) but WordPress requires them defined.
define('DB_NAME', 'wordpress');
define('DB_USER', 'root');
define('DB_PASSWORD', '');
define('DB_HOST', 'localhost');
define('DB_CHARSET', 'utf8');
define('DB_COLLATE', '');
define('AUTH_KEY',         'wk-demo-auth-key-not-secret-0001');
define('SECURE_AUTH_KEY',  'wk-demo-secure-auth-key-0002');
define('LOGGED_IN_KEY',    'wk-demo-logged-in-key-0003');
define('NONCE_KEY',        'wk-demo-nonce-key-0004');
define('AUTH_SALT',        'wk-demo-auth-salt-0005');
define('SECURE_AUTH_SALT', 'wk-demo-secure-auth-salt-0006');
define('LOGGED_IN_SALT',   'wk-demo-logged-in-salt-0007');
define('NONCE_SALT',       'wk-demo-nonce-salt-0008');
\$table_prefix = 'wp_';
// Pin the site URL to the HostPort so links/redirects stay on localhost:$PORT.
define('WP_HOME', '$WP_URL');
define('WP_SITEURL', '$WP_URL');
define('WP_DEBUG', false);
define('AUTOMATIC_UPDATER_DISABLED', true);
define('WP_AUTO_UPDATE_CORE', false);
define('FS_METHOD', 'direct');
if ( ! defined('ABSPATH') ) define('ABSPATH', __DIR__ . '/');
require_once ABSPATH . 'wp-settings.php';
PHP

echo "done. Now:  wk run example/wordpress.wk   then open $WP_URL"
