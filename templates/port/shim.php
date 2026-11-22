<?php
// rcargo port shim for `{{FUNCTION}}` (PHP 8.0+, no extensions beyond json/mbstring).
//
// In your app (the only edit to the original source; undo by renaming the function back):
//
//   require_once "<path to this shims folder>/rcargo_port.php";    // require_once: the CLI below loads it too
//   function your_function__php($a, $b) { ... }                       // the original, renamed
//   function your_function(...$args) { return RcargoPort::call('your_function__php', $args); }
//
// Behaviour (positional arguments only):
//   RCARGO_PORT_RECORD=<file>  run the original and append the call to <file> (golden cases)
//   RCARGO_PORT_DISABLE=1      always run the original
//   otherwise                  ask the Rust binary; fall back to the original when the binary is
//                              missing, arguments are not plain JSON data, or the port reports `fatal`.
// Results are plain JSON types (objects come back as associative arrays). When the port reports an
// error, the original runs so its own exception is thrown.
//
// Command line (FUNCTION is `name` or `Class::method`; MODULE is the PHP file defining it):
//   php rcargo_port.php oracle MODULE.php FUNCTION CANDIDATES.jsonl CASES.jsonl
//   php rcargo_port.php bench  MODULE.php FUNCTION CASES.jsonl

final class RcargoPort
{
    private static $proc = null;
    private static $pipes = [];
    private static bool $broken = false;
    private static array $seen = [];
    private const FLAGS = JSON_UNESCAPED_UNICODE | JSON_UNESCAPED_SLASHES | JSON_PRESERVE_ZERO_FRACTION | JSON_THROW_ON_ERROR;

    public static function bin(): string
    {
        $env = getenv('RCARGO_PORT_BIN');
        return $env !== false && $env !== '' ? $env : __DIR__ . '/../target/release/{{BIN}}';
    }

    private static function plain($v): bool
    {
        if ($v === null || is_bool($v) || is_int($v)) return true;
        if (is_float($v)) return is_finite($v);
        if (is_string($v)) return mb_check_encoding($v, 'UTF-8');
        if (is_array($v)) {
            foreach ($v as $k => $x) {
                if (is_string($k) && !mb_check_encoding($k, 'UTF-8')) return false;
                if (!self::plain($x)) return false;
            }
            return true;
        }
        return false;
    }

    private static function ask(string $request): array
    {
        if (self::$proc === null) {
            self::$proc = proc_open([self::bin()], [0 => ['pipe', 'r'], 1 => ['pipe', 'w']], self::$pipes);
            if (!is_resource(self::$proc)) {
                self::$proc = null;
                throw new RuntimeException('cannot start port binary');
            }
        }
        if (fwrite(self::$pipes[0], $request . "\n") === false || fflush(self::$pipes[0]) === false) {
            throw new RuntimeException('port binary closed');
        }
        $line = fgets(self::$pipes[1]);
        if ($line === false) throw new RuntimeException('port binary exited');
        return json_decode($line, true, 512, JSON_THROW_ON_ERROR);
    }

    private static function accelerated(callable $orig, array $args)
    {
        if (self::$broken || !is_file(self::bin()) || !self::plain($args)) return $orig(...$args);
        try {
            $res = self::ask('{"args":' . json_encode($args, self::FLAGS) . '}');
        } catch (Throwable $e) {
            self::$broken = true;
            return $orig(...$args);
        }
        if (array_key_exists('ok', $res)) return $res['ok'];
        // 'err': fall through, so the original throws its own exception.
        return $orig(...$args);
    }

    private static function caseLine(string $key, ?Throwable $err, $result, int $ns): ?string
    {
        if ($err === null && !self::plain($result)) return null;
        $expect = $err === null ? ['ok' => $result] : ['err' => $err->getMessage()];
        return '{"args":' . $key . ',"expect":' . json_encode($expect, self::FLAGS) . ',"ns":' . $ns . '}';
    }

    private static function record(callable $orig, array $args, string $path)
    {
        if (!isset(self::$seen[$path])) {
            self::$seen[$path] = [];
            if (is_file($path)) {
                foreach (file($path, FILE_IGNORE_NEW_LINES | FILE_SKIP_EMPTY_LINES) as $l) {
                    self::$seen[$path][json_encode(json_decode($l, true)['args'], self::FLAGS)] = true;
                }
            }
        }
        if (!self::plain($args)) return $orig(...$args);
        $key = json_encode($args, self::FLAGS);
        $max = (int)(getenv('RCARGO_PORT_MAX_CASES') ?: 500);
        if (count(self::$seen[$path]) >= $max || isset(self::$seen[$path][$key])) return $orig(...$args);
        $t0 = hrtime(true);
        $err = null;
        $result = null;
        try {
            $result = $orig(...json_decode($key, true));
        } catch (Exception $e) {
            $err = $e;
        }
        $line = self::caseLine($key, $err, $result, hrtime(true) - $t0);
        if ($line !== null) {
            @mkdir(dirname($path), 0777, true);
            file_put_contents($path, $line . "\n", FILE_APPEND);
            self::$seen[$path][$key] = true;
        }
        if ($err !== null) throw $err;
        return $result;
    }

    public static function call(callable $orig, array $args)
    {
        if (getenv('RCARGO_PORT_DISABLE') === '1') return $orig(...$args);
        $path = getenv('RCARGO_PORT_RECORD');
        if ($path !== false && $path !== '') return self::record($orig, $args, $path);
        return self::accelerated($orig, $args);
    }

    // ---- command line ---------------------------------------------------------------------
    public static function cases(string $file): array
    {
        return array_map(fn($l) => json_decode($l, true, 512, JSON_THROW_ON_ERROR), file($file, FILE_IGNORE_NEW_LINES | FILE_SKIP_EMPTY_LINES));
    }

    public static function oracle(string $module, string $function, string $candidates, string $out): void
    {
        putenv('RCARGO_PORT_DISABLE=1');
        putenv('RCARGO_PORT_RECORD');
        require_once realpath($module);
        $have = [];
        if (is_file($out)) foreach (self::cases($out) as $c) $have[json_encode($c['args'], self::FLAGS)] = true;
        @mkdir(dirname($out), 0777, true);
        $added = $errors = 0;
        $f = fopen($out, 'a');
        foreach (self::cases($candidates) as $c) {
            $key = json_encode($c['args'], self::FLAGS);
            if (isset($have[$key])) continue;
            $t0 = hrtime(true);
            $err = null;
            $result = null;
            try {
                $result = $function(...json_decode($key, true));
            } catch (Exception $e) {
                $err = $e;
            }
            $line = self::caseLine($key, $err, $result, hrtime(true) - $t0);
            if ($line === null) continue;
            fwrite($f, $line . "\n");
            $have[$key] = true;
            $added++;
            if ($err !== null) $errors++;
        }
        fclose($f);
        fwrite(STDERR, "oracle: $added new cases ($errors are errors)\n");
    }

    public static function bench(string $module, string $function, string $file): void
    {
        putenv('RCARGO_PORT_DISABLE=1');
        require_once realpath($module);
        if (!is_file(self::bin())) { fwrite(STDERR, 'no port binary at ' . self::bin() . ": run `rcargo port build` first\n"); exit(1); }
        $calls = array_map(fn($c) => $c['args'], self::cases($file));
        if (!$calls) { fwrite(STDERR, "no cases in $file\n"); exit(1); }
        $reqs = array_map(fn($a) => '{"args":' . json_encode($a, self::FLAGS) . '}', $calls);
        $best = function (callable $run): float {
            $b = INF;
            for ($i = 0; $i < 3; $i++) { $t0 = hrtime(true); $run(); $b = min($b, (hrtime(true) - $t0) / 1e9); }
            return $b;
        };
        $tOrig = $best(function () use ($function, $calls) { foreach ($calls as $a) { try { $function(...$a); } catch (Throwable $e) {} } });
        $tPort = $best(function () use ($reqs) { foreach ($reqs as $r) self::ask($r); });
        printf("original %.1fms, port %.1fms: %.1fx per call set (%d calls, best of 3, bridge included)\n", $tOrig * 1e3, $tPort * 1e3, $tOrig / $tPort, count($calls));
        if ($tPort >= $tOrig) echo "the port is not faster end to end: do not enable it (the call crosses the bridge too often, or the work is too small)\n";
    }
}

if (PHP_SAPI === 'cli' && isset($_SERVER['SCRIPT_FILENAME']) && realpath($_SERVER['SCRIPT_FILENAME']) === __FILE__) {
    $a = array_slice($argv, 1);
    if (($a[0] ?? '') === 'oracle' && count($a) === 5) RcargoPort::oracle(...array_slice($a, 1));
    elseif (($a[0] ?? '') === 'bench' && count($a) === 4) RcargoPort::bench(...array_slice($a, 1));
    else { fwrite(STDERR, "usage: rcargo_port.php oracle MODULE.php FUNCTION CANDIDATES.jsonl CASES.jsonl | bench MODULE.php FUNCTION CASES.jsonl\n"); exit(2); }
}
