<?php
/**
 * Talus HTTP tunnel for shared hosting.
 *
 * Carries the *unchanged* wire protocol of a database (MySQL, PostgreSQL, SQL Server, Oracle, Redis, ...)
 * between Talus and a database server that only this web host can reach. Talus opens a local port; every
 * connection to it becomes one database *stream* inside one *tunnel*, carried by HTTPS requests to this script:
 *
 *   open   one long request per tunnel. This PHP process stays alive, listens on a private unix socket for the
 *          other requests and owns every database connection of the tunnel. It ends when the tunnel is
 *          closed, idle, or Talus drops the request.
 *   dial   opens a stream: the owner connects to host:port (X-Talus-Target, checked against 'allow')
 *   send   bytes from Talus to a stream's database       (X-Talus-Sid, body = the bytes)
 *   recv   what the databases sent, long-poll, all streams in one answer (X-Talus-Wait = milliseconds)
 *   close  ends one stream;  quit  ends the tunnel;  ping  authenticated self-test
 *
 * Because the real connections live here, transactions span requests, a running statement can be cancelled
 * with the database's own cancel protocol (Talus opens another stream for it), and the database password is
 * never sent to this script: the login handshake travels through it like any other bytes. Whatever the number
 * of streams, a tunnel occupies two PHP workers (the owner and the waiting recv) plus one per request in flight.
 *
 * SETUP: copy this file to the web host, set 'key' (32+ random characters) and 'allow' below, enter the
 * same key and this script's https:// address in Talus. See docs/php-tunnel.md for requirements and limits.
 */

$CONFIG = array(
    // Shared secret (or set the TALUS_TUNNEL_KEY environment variable instead).
    // Generate one with: php -r "echo bin2hex(random_bytes(24)), PHP_EOL;"
    'key' => 'CHANGE-ME',

    // Database servers this script may connect to, as "host:port" ("*" = any port of that host).
    // Everything else is refused, so the script is never an open relay to your network.
    // (TALUS_TUNNEL_ALLOW="host:port,host:port" in the environment replaces this list.)
    'allow' => array('127.0.0.1:3306', '127.0.0.1:5432', 'localhost:3306', 'localhost:5432'),

    // Plain http:// is accepted only from loopback (testing). Set true to accept it everywhere (not advised:
    // the signed requests are not encrypted).
    'allow_plain_http' => false,

    'max_tunnels'   => 4,        // tunnels (Talus connections) open at once
    'max_streams'   => 16,       // database connections per tunnel
    'idle_timeout'  => 300,      // seconds without any request before a tunnel is dropped
    'max_lifetime'  => 28800,    // seconds a tunnel may live at most
    'max_chunk'     => 1048576,  // bytes per send / recv request
    'max_wait_ms'   => 10000,    // longest recv long-poll (keep below the host's request timeout)
    'clock_skew'    => 120,      // seconds a request's timestamp may differ from this server's clock
    'connect_timeout' => 10,     // seconds to reach a database
    'tmp_dir'       => '',       // '' = the system temp directory
);

// ---------------------------------------------------------------------------------------------------------

const TALUS_PROTO = 'talus2';
/** Bytes buffered per stream before the database is left waiting (TCP back-pressure). */
const TALUS_STREAM_BUFFER = 2097152;
const TALUS_NO_STREAM = '00000000000000000000000000000000';

function fail($code, $message)
{
    if (!headers_sent()) {
        http_response_code($code);
        header('Content-Type: application/json');
        header('Cache-Control: no-store');
        header('X-Talus-Status: error');
    }
    echo json_encode(array('error' => $message));
    exit;
}

function hdr($name)
{
    $key = 'HTTP_' . strtoupper(str_replace('-', '_', $name));
    return isset($_SERVER[$key]) ? (string) $_SERVER[$key] : '';
}

function tmp_dir($cfg)
{
    $d = $cfg['tmp_dir'] !== '' ? $cfg['tmp_dir'] : sys_get_temp_dir();
    return rtrim($d, '/\\');
}

function is_loopback($addr)
{
    return $addr === '127.0.0.1' || $addr === '::1';
}

/** The key: the TALUS_TUNNEL_KEY environment variable wins over the CONFIG block (keeps it out of the file). */
function effective_key($cfg)
{
    $env = getenv('TALUS_TUNNEL_KEY');
    return ($env !== false && $env !== '') ? $env : $cfg['key'];
}

/** Plain http:// from non-loopback clients: the CONFIG flag, or TALUS_TUNNEL_ALLOW_HTTP=1 (tests, trusted LANs). */
function allow_plain_http($cfg)
{
    return $cfg['allow_plain_http'] || getenv('TALUS_TUNNEL_ALLOW_HTTP') === '1';
}

/** Allowed targets: TALUS_TUNNEL_ALLOW ("host:port,host:*") wins over the CONFIG block. */
function effective_allow($cfg)
{
    $env = getenv('TALUS_TUNNEL_ALLOW');
    if ($env !== false && $env !== '') {
        return array_values(array_filter(array_map('trim', explode(',', $env))));
    }
    return $cfg['allow'];
}

/** True when `host:port` is allowed by a pattern list. */
function target_allowed($cfg, $host, $port)
{
    $host = strtolower($host);
    foreach (effective_allow($cfg) as $pattern) {
        $p = strrpos($pattern, ':');
        if ($p === false) {
            continue;
        }
        $h = strtolower(substr($pattern, 0, $p));
        $pt = substr($pattern, $p + 1);
        if ($h === $host && ($pt === '*' || (string) $port === $pt)) {
            return true;
        }
    }
    return false;
}

/** Splits "host:port" (or "[v6]:port"); null when malformed. */
function parse_target($t)
{
    if (!preg_match('/^(\[[0-9a-fA-F:.]+\]|[A-Za-z0-9._-]{1,253}):([0-9]{1,5})$/', $t, $m)) {
        return null;
    }
    $port = (int) $m[2];
    if ($port < 1 || $port > 65535) {
        return null;
    }
    return array(trim($m[1], '[]'), $port);
}

function check_environment()
{
    $missing = array();
    foreach (array('hash_hmac', 'hash_equals', 'stream_socket_client', 'stream_socket_server', 'stream_select', 'random_bytes') as $f) {
        if (!function_exists($f)) {
            $missing[] = $f;
        }
    }
    return $missing;
}

/** Verifies the request signature; returns the validated request fields. */
function authenticate($cfg)
{
    $key = effective_key($cfg);
    if ($key === 'CHANGE-ME' || strlen($key) < 32) {
        error_log('talus-tunnel: set a key of at least 32 characters in the CONFIG block');
        fail(503, 'the tunnel is not configured');
    }
    if ($_SERVER['REQUEST_METHOD'] !== 'POST') {
        fail(405, 'POST only');
    }
    $https = (!empty($_SERVER['HTTPS']) && $_SERVER['HTTPS'] !== 'off')
        || strtolower(hdr('X-Forwarded-Proto')) === 'https';
    $remote = isset($_SERVER['REMOTE_ADDR']) ? $_SERVER['REMOTE_ADDR'] : '';
    if (!$https && !allow_plain_http($cfg) && !is_loopback($remote)) {
        fail(403, 'HTTPS is required');
    }
    $op = hdr('X-Talus-Op');
    $tid = hdr('X-Talus-Tid');
    $sid = hdr('X-Talus-Sid');
    $ts = hdr('X-Talus-Ts');
    $nonce = hdr('X-Talus-Nonce');
    $seq = hdr('X-Talus-Seq');
    $target = hdr('X-Talus-Target');
    $sig = hdr('X-Talus-Sig');
    if (!in_array($op, array('open', 'dial', 'send', 'recv', 'close', 'quit', 'ping'), true)
        || !preg_match('/^[0-9a-f]{32}$/', $tid)
        || !preg_match('/^[0-9a-f]{32}$/', $sid)
        || !preg_match('/^[0-9]{1,12}$/', $ts)
        || !preg_match('/^[0-9a-f]{32}$/', $nonce)
        || !preg_match('/^[0-9]{1,15}$/', $seq === '' ? '0' : $seq)
        || !preg_match('/^[0-9a-f]{64}$/', $sig)) {
        fail(400, 'malformed request');
    }
    $body = file_get_contents('php://input', false, null, 0, $cfg['max_chunk'] + 1);
    if ($body === false) {
        $body = '';
    }
    if (strlen($body) > $cfg['max_chunk']) {
        fail(413, 'request too large');
    }
    $material = TALUS_PROTO . "\n" . $op . "\n" . $tid . "\n" . $sid . "\n" . $ts . "\n" . $nonce . "\n" . $seq . "\n"
        . $target . "\n" . hash('sha256', $body);
    $expected = hash_hmac('sha256', $material, $key);
    if (!hash_equals($expected, $sig)) {
        usleep(200000); // slows guessing; the key is long enough that this is only a courtesy
        fail(401, 'bad signature');
    }
    if (abs(time() - (int) $ts) > $cfg['clock_skew']) {
        fail(401, 'the request is too old or the clocks differ');
    }
    return array('op' => $op, 'tid' => $tid, 'sid' => $sid, 'nonce' => $nonce, 'seq' => (int) $seq,
        'target' => $target, 'body' => $body, 'ts' => (int) $ts);
}

/** A request may be used once: its nonce becomes a marker file for as long as its timestamp is valid. */
function consume_nonce($cfg, $nonce)
{
    $dir = tmp_dir($cfg);
    $f = $dir . '/talus-nonce-' . $nonce;
    $h = @fopen($f, 'x');
    if ($h === false) {
        fail(401, 'replayed request');
    }
    fclose($h);
    // Tidy up markers that can no longer matter (best effort, at most a few per request).
    $old = time() - 2 * $cfg['clock_skew'] - 5;
    $n = 0;
    foreach ((array) glob($dir . '/talus-nonce-*') as $x) {
        if ($n++ > 50) {
            break;
        }
        if (@filemtime($x) < $old) {
            @unlink($x);
        }
    }
}

function sock_path($cfg, $tid)
{
    return tmp_dir($cfg) . '/talus-' . $tid . '.sock';
}

function live_tunnels($cfg)
{
    $n = 0;
    foreach ((array) glob(tmp_dir($cfg) . '/talus-*.sock') as $p) {
        $c = @stream_socket_client('unix://' . $p, $e, $s, 1);
        if ($c) {
            fclose($c);
            $n++;
        } else {
            @unlink($p); // left behind by a process that was killed
        }
    }
    return $n;
}

// --- frames between request processes and the owner -------------------------------------------------------

function read_exact($s, $n, $deadline)
{
    $out = '';
    while (strlen($out) < $n) {
        $left = $deadline - microtime(true);
        if ($left <= 0) {
            return null;
        }
        $r = array($s);
        $w = null;
        $x = null;
        if (@stream_select($r, $w, $x, (int) $left, (int) (($left - floor($left)) * 1000000)) < 1) {
            continue;
        }
        $chunk = fread($s, $n - strlen($out));
        if ($chunk === false || ($chunk === '' && feof($s))) {
            return null;
        }
        $out .= $chunk;
    }
    return $out;
}

function write_all($s, $data, $deadline)
{
    $len = strlen($data);
    $off = 0;
    while ($off < $len) {
        if (microtime(true) > $deadline) {
            return false;
        }
        $n = @fwrite($s, substr($data, $off, 65536));
        if ($n === false) {
            return false;
        }
        if ($n === 0) {
            $r = null;
            $w = array($s);
            $x = null;
            @stream_select($r, $w, $x, 0, 200000);
            continue;
        }
        $off += $n;
    }
    return true;
}

/**
 * Request frame: op(1) sid(16 raw) seq(8) wait_ms(4) len(4) payload. Reply: status(1) len(4) payload.
 * Status 0 ok, 1 stream ended, 2 error (payload = message).
 */
function ipc_call($cfg, $tid, $op, $sid, $seq, $wait, $payload, $timeout)
{
    $path = sock_path($cfg, $tid);
    $deadline = microtime(true) + 3;
    $c = false;
    while (!$c && microtime(true) < $deadline) {
        $c = @stream_socket_client('unix://' . $path, $e, $s, 1);
        if (!$c) {
            usleep(50000); // the owner may only just have started
        }
    }
    if (!$c) {
        return array(2, 'no such tunnel');
    }
    $frame = $op . hex2bin($sid) . pack('J', $seq) . pack('N', $wait) . pack('N', strlen($payload)) . $payload;
    $end = microtime(true) + $timeout;
    if (!write_all($c, $frame, $end)) {
        fclose($c);
        return array(2, 'the tunnel is gone');
    }
    $head = read_exact($c, 5, $end);
    if ($head === null) {
        fclose($c);
        return array(2, 'the tunnel did not answer');
    }
    $status = ord($head[0]);
    $len = unpack('N', substr($head, 1, 4));
    $len = $len[1];
    $data = $len > 0 ? read_exact($c, $len, $end) : '';
    fclose($c);
    if ($data === null) {
        return array(2, 'the tunnel did not answer');
    }
    return array($status, $data);
}

// --- the owner: one long request that keeps the database connections ---------------------------------------

/** Opens a stream; returns array(true, resource) or array(false, message). */
function dial($cfg, $target)
{
    $t = parse_target($target);
    if ($t === null) {
        return array(false, 'malformed target');
    }
    list($host, $port) = $t;
    if (!target_allowed($cfg, $host, $port)) {
        error_log('talus-tunnel: refused target ' . $target);
        return array(false, 'this target is not allowed by the tunnel configuration');
    }
    $addr = (strpos($host, ':') !== false) ? '[' . $host . ']' : $host;
    $db = @stream_socket_client('tcp://' . $addr . ':' . $port, $errno, $errstr, $cfg['connect_timeout']);
    if (!$db) {
        error_log('talus-tunnel: connect to ' . $target . ' failed: ' . $errstr);
        return array(false, 'cannot reach the database server (' . ($errno ? 'error ' . $errno : 'timeout') . ')');
    }
    stream_set_blocking($db, false);
    return array(true, $db);
}

function run_owner($cfg, $req)
{
    if (live_tunnels($cfg) >= $cfg['max_tunnels']) {
        fail(429, 'too many open tunnels');
    }
    $path = sock_path($cfg, $req['tid']);
    @unlink($path);
    $old = umask(0177);
    $server = @stream_socket_server('unix://' . $path, $e, $es);
    umask($old);
    if (!$server) {
        error_log('talus-tunnel: cannot create the tunnel socket: ' . $es);
        fail(500, 'the host does not allow the tunnel to open its socket');
    }
    @chmod($path, 0600);

    ignore_user_abort(true);
    @set_time_limit(0);
    while (ob_get_level() > 0) {
        @ob_end_flush();
    }
    @ini_set('zlib.output_compression', '0');
    @ini_set('output_buffering', '0');
    header('Content-Type: application/octet-stream');
    header('Cache-Control: no-store');
    header('X-Accel-Buffering: no');
    header('X-Talus-Status: open');
    echo "OK\n";
    flush();

    $streams = array();      // sid(hex) => array('db' => resource, 'buf' => string, 'eof' => bool, 'told' => bool, ...)
    $started = time();
    $last_activity = time();
    $last_beat = time();
    $seen_recv = -1;
    $recv_reply = '';
    // The one `recv` request that is waiting for data: array(stream, seq, deadline). Waiting must not block
    // the loop: a `send` arrives on its own request while a database is still thinking.
    $pending = null;

    $drain = function () use (&$streams) {
        foreach ($streams as $sid => &$st) {
            if ($st['eof']) {
                continue;
            }
            while (strlen($st['buf']) < TALUS_STREAM_BUFFER) {
                $d = @fread($st['db'], 65536);
                if ($d === false || ($d === '' && feof($st['db']))) {
                    $st['eof'] = true;
                    break;
                }
                if ($d === '') {
                    break;
                }
                $st['buf'] .= $d;
            }
        }
        unset($st);
    };
    $has_output = function () use (&$streams) {
        foreach ($streams as $st) {
            if ($st['buf'] !== '' || ($st['eof'] && !$st['told'])) {
                return true;
            }
        }
        return false;
    };
    // Packs what the databases sent: frames sid(16) flags(1) len(4) data, flags bit 0 = the stream ended.
    $collect = function () use (&$streams, $cfg) {
        $out = '';
        foreach ($streams as $sid => &$st) {
            $room = $cfg['max_chunk'] - strlen($out);
            if ($room <= 0) {
                break;
            }
            $data = substr($st['buf'], 0, $room);
            $st['buf'] = (string) substr($st['buf'], strlen($data));
            $ended = $st['eof'] && $st['buf'] === '';
            if ($data === '' && !($ended && !$st['told'])) {
                continue;
            }
            $out .= hex2bin($sid) . chr($ended ? 1 : 0) . pack('N', strlen($data)) . $data;
            if ($ended) {
                $st['told'] = true;
            }
        }
        unset($st);
        foreach ($streams as $sid => $st) {
            if ($st['told']) {
                @fclose($st['db']);
                unset($streams[$sid]);
            }
        }
        return $out;
    };
    // Hands the collected output to a waiting recv (or an empty answer when `$empty`).
    $answer = function ($p, $empty) use ($collect, &$seen_recv, &$recv_reply) {
        $out = $empty ? '' : $collect();
        if (!$empty) {
            $seen_recv = $p[1];
            $recv_reply = $out;
        }
        write_all($p[0], chr(0) . pack('N', strlen($out)) . $out, microtime(true) + 30);
        fclose($p[0]);
    };

    while (true) {
        $drain();
        if ($pending !== null && ($has_output() || microtime(true) >= $pending[2])) {
            $answer($pending, false);
            $pending = null;
        }
        $r = array($server);
        foreach ($streams as $st) {
            if (!$st['eof'] && strlen($st['buf']) < TALUS_STREAM_BUFFER) {
                $r[] = $st['db'];
            }
        }
        $w = null;
        $x = null;
        $ready = @stream_select($r, $w, $x, 0, $pending !== null ? 200000 : 1000000);
        if ($pending !== null) {
            $last_activity = time();
        }
        if ($ready > 0 && in_array($server, $r, true)) {
            $c = @stream_socket_accept($server, 0);
            if ($c) {
                $end = microtime(true) + 30;
                $head = read_exact($c, 33, $end);
                if ($head === null) {
                    fclose($c);
                    continue;
                }
                $op = $head[0];
                $sid = bin2hex(substr($head, 1, 16));
                $seq = unpack('J', substr($head, 17, 8));
                $seq = $seq[1];
                $wait = unpack('N', substr($head, 25, 4));
                $wait = $wait[1];
                $len = unpack('N', substr($head, 29, 4));
                $len = $len[1];
                $payload = $len > 0 ? read_exact($c, $len, $end) : '';
                $last_activity = time();
                $status = 0;
                $out = '';
                if ($payload === null) {
                    $status = 2;
                    $out = 'truncated request';
                } elseif ($op === 'D') {
                    if (isset($streams[$sid])) {
                        // a retry of a dial that already went through
                    } elseif (count($streams) >= $cfg['max_streams']) {
                        $status = 2;
                        $out = 'too many open connections in this tunnel';
                    } else {
                        list($ok, $res) = dial($cfg, $payload);
                        if ($ok) {
                            $streams[$sid] = array('db' => $res, 'buf' => '', 'eof' => false, 'told' => false, 'seen_send' => -1);
                        } else {
                            $status = 2;
                            $out = $res;
                        }
                    }
                } elseif ($op === 'S') {
                    if (!isset($streams[$sid])) {
                        $status = 1; // the stream ended (or never existed)
                    } elseif ($seq === $streams[$sid]['seen_send']) {
                        // a retry of the request that already went through
                    } elseif ($seq < $streams[$sid]['seen_send']) {
                        $status = 2;
                        $out = 'out-of-order request';
                    } elseif ($streams[$sid]['eof']) {
                        $status = 1;
                    } else {
                        $db = $streams[$sid]['db'];
                        $ok = true;
                        $off = 0;
                        $stop = microtime(true) + 60;
                        while ($off < strlen($payload)) {
                            $n = @fwrite($db, substr($payload, $off, 65536));
                            if ($n === false) {
                                $ok = false;
                                break;
                            }
                            if ($n === 0) {
                                if (microtime(true) > $stop) {
                                    $ok = false;
                                    break;
                                }
                                $dr = array($db);
                                $dw = array($db);
                                $dx = null;
                                @stream_select($dr, $dw, $dx, 0, 100000);
                                $drain();
                                continue;
                            }
                            $off += $n;
                        }
                        if ($ok) {
                            $streams[$sid]['seen_send'] = $seq;
                        } else {
                            $status = 2;
                            $out = 'the database connection broke';
                            $streams[$sid]['eof'] = true;
                        }
                    }
                } elseif ($op === 'R') {
                    if ($seq === $seen_recv) {
                        $out = $recv_reply; // a retry: the answer the client never received
                    } elseif ($seq < $seen_recv) {
                        $status = 2;
                        $out = 'out-of-order request';
                    } else {
                        if ($pending !== null) {
                            $answer($pending, true); // a newer recv replaces an abandoned one
                            $pending = null;
                        }
                        $drain();
                        if ($has_output() || $wait <= 0) {
                            $answer(array($c, $seq, 0), false);
                            continue;
                        }
                        $pending = array($c, $seq, microtime(true) + min($wait, $cfg['max_wait_ms']) / 1000);
                        continue;
                    }
                } elseif ($op === 'X') {
                    if (isset($streams[$sid])) {
                        @fclose($streams[$sid]['db']);
                        unset($streams[$sid]);
                    }
                } elseif ($op === 'Q') {
                    write_all($c, chr(0) . pack('N', 0), microtime(true) + 5);
                    fclose($c);
                    break;
                } else {
                    $status = 2;
                    $out = 'unknown request';
                }
                write_all($c, chr($status) . pack('N', strlen($out)) . $out, microtime(true) + 30);
                fclose($c);
            }
        }
        $now = time();
        if ($now - $last_activity > $cfg['idle_timeout'] || $now - $started > $cfg['max_lifetime']) {
            break;
        }
        if ($now - $last_beat >= 10) {
            // Keeps proxies from timing the request out, and notices that Talus went away.
            echo ' ';
            flush();
            $last_beat = $now;
            if (connection_aborted()) {
                break;
            }
        }
    }
    if ($pending !== null) {
        $answer($pending, true);
    }
    foreach ($streams as $st) {
        @fclose($st['db']);
    }
    @fclose($server);
    @unlink($path);
    echo "\nEND\n";
    flush();
    exit;
}

// --- request dispatch -------------------------------------------------------------------------------------

function respond($status, $data, $code = 200)
{
    http_response_code($code);
    header('Content-Type: application/octet-stream');
    header('Cache-Control: no-store');
    header('X-Talus-Status: ' . $status);
    header('Content-Length: ' . strlen($data));
    echo $data;
    exit;
}

function main($cfg)
{
    $missing = check_environment();
    $req = authenticate($cfg);
    if ($missing) {
        fail(501, 'this PHP lacks: ' . implode(', ', $missing));
    }
    consume_nonce($cfg, $req['nonce']);
    switch ($req['op']) {
        case 'ping':
            header('Content-Type: application/json');
            header('Cache-Control: no-store');
            header('X-Talus-Status: ok');
            echo json_encode(array(
                'tunnel' => TALUS_PROTO,
                'php' => PHP_VERSION,
                'sapi' => PHP_SAPI,
                'open_tunnels' => live_tunnels($cfg),
                'max_tunnels' => $cfg['max_tunnels'],
                'max_streams' => $cfg['max_streams'],
                'max_wait_ms' => $cfg['max_wait_ms'],
                'time_limit' => (int) ini_get('max_execution_time'),
                'can_extend_time' => function_exists('set_time_limit') && stripos((string) ini_get('disable_functions'), 'set_time_limit') === false,
                'allowed_targets' => count(effective_allow($cfg)),
            ));
            exit;
        case 'open':
            run_owner($cfg, $req);
            return;
        case 'dial':
            $t = parse_target($req['target']);
            if ($t === null) {
                fail(400, 'malformed target');
            }
            if (!target_allowed($cfg, $t[0], $t[1])) {
                error_log('talus-tunnel: refused target ' . $req['target']);
                fail(403, 'this target is not allowed by the tunnel configuration');
            }
            list($st, $data) = ipc_call($cfg, $req['tid'], 'D', $req['sid'], $req['seq'], 0, $req['target'], $cfg['connect_timeout'] + 10);
            if ($st === 2) {
                fail($data === 'no such tunnel' ? 410 : 502, $data);
            }
            respond('ok', '');
            return;
        case 'send':
            list($st, $data) = ipc_call($cfg, $req['tid'], 'S', $req['sid'], $req['seq'], 0, $req['body'], 70);
            break;
        case 'recv':
            $wait = max(0, (int) hdr('X-Talus-Wait'));
            list($st, $data) = ipc_call($cfg, $req['tid'], 'R', TALUS_NO_STREAM, $req['seq'], $wait, '', min($wait, $cfg['max_wait_ms']) / 1000 + 10);
            break;
        case 'close':
            list($st, $data) = ipc_call($cfg, $req['tid'], 'X', $req['sid'], $req['seq'], 0, '', 10);
            break;
        default: // quit
            list($st, $data) = ipc_call($cfg, $req['tid'], 'Q', TALUS_NO_STREAM, $req['seq'], 0, '', 10);
            break;
    }
    if ($st === 2) {
        fail($data === 'no such tunnel' ? 410 : 502, $data);
    }
    respond($st === 1 ? 'closed' : 'ok', $data);
}

main($CONFIG);
