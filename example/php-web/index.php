<?php
// A real PHP page served by `php -S` running as a wasm container on wk's
// network fabric. Bind-mount this dir at /app and serve it on a HostPort.
header("Content-Type: text/html; charset=utf-8");
$n = (int)(@file_get_contents("/tmp/hits") ?: 0) + 1;
@file_put_contents("/tmp/hits", $n);
?>
<!doctype html>
<title>PHP on wk</title>
<h1>Hello from PHP <?= PHP_VERSION ?> 🐘</h1>
<p>Served by PHP's built-in <code>-S</code> webserver, compiled to
<code>wasm32-wasip2</code>, talking real TCP over wk's userspace fabric.</p>
<ul>
  <li>request URI: <code><?= htmlspecialchars($_SERVER['REQUEST_URI']) ?></code></li>
  <li>request #: <?= $n ?></li>
  <li>SAPI: <code><?= php_sapi_name() ?></code></li>
  <li>time: <?= date('c') ?></li>
</ul>
