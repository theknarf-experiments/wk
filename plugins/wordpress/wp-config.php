<?php
// wk demo WordPress on SQLite. The MySQL constants are unused (the wp-content/
// db.php drop-in routes to SQLite) but WordPress requires them defined.
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
$table_prefix = 'wp_';
// Pin the site URL to the HostPort so links/redirects stay on localhost:8092.
define('WP_HOME', 'http://localhost:8092');
define('WP_SITEURL', 'http://localhost:8092');
define('WP_DEBUG', false);
define('AUTOMATIC_UPDATER_DISABLED', true);
define('WP_AUTO_UPDATE_CORE', false);
define('FS_METHOD', 'direct');
if ( ! defined('ABSPATH') ) define('ABSPATH', __DIR__ . '/');
require_once ABSPATH . 'wp-settings.php';
